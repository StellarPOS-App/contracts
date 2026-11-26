//! Merchant registry.
//!
//! The registry is the source of truth the payment processor consults before
//! it moves any funds: a merchant must be registered, approved by the registry
//! admin, not suspended, and must list the asset being paid in.
//!
//! Lifecycle: `register` (Pending) -> `approve` (Active) <-> `suspend`
//! (Suspended) -> `reinstate` (Active). Merchants manage their own settlement
//! account and accepted-asset list; only the admin changes status.

#![no_std]

use shared::{extend_instance, extend_persistent};
use soroban_sdk::{
    contract, contractclient, contracterror, contractevent, contractimpl, contracttype, Address,
    Env, String, Vec,
};

#[contractclient(name = "EscrowClient")]
pub trait EscrowInterface {
    fn cancel_for_merchant(env: Env, merchant: Address) -> u32;
}

/// Maximum number of assets a merchant can accept. Keeps `accepts` cheap.
pub const MAX_ACCEPTED_ASSETS: u32 = 10;
pub const MAX_NAME_LEN: u32 = 64;
/// Cap on merchants per approve_many call, to stay well inside resource limits.
pub const MAX_BATCH: u32 = 25;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum RegistryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Paused = 3,
    AlreadyRegistered = 4,
    MerchantNotFound = 5,
    InvalidName = 6,
    TooManyAssets = 7,
    DuplicateAsset = 8,
    InvalidStatusTransition = 9,
    BatchTooLarge = 10,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MerchantStatus {
    Pending,
    Active,
    Suspended,
    Deregistered,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Merchant {
    /// Account that controls the merchant profile.
    pub owner: Address,
    pub name: String,
    /// Account that receives net sale proceeds. May differ from `owner`
    /// (e.g. a cold wallet or a business bank anchor account).
    pub settlement: Address,
    pub accepted_assets: Vec<Address>,
    pub status: MerchantStatus,
    pub registered_at: u64,
    pub updated_at: u64,
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Admin,
    Paused,
    MerchantCount,
    Merchant(Address),
    EscrowService,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MerchantRegistered {
    #[topic]
    pub owner: Address,
    pub name: String,
    pub settlement: Address,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MerchantStatusChanged {
    #[topic]
    pub owner: Address,
    pub status: MerchantStatus,
}

/// One compact event for an administrative batch operation. Individual
/// status events are intentionally not emitted by `approve_many`.
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MerchantBatchStatusChanged {
    pub requested: u32,
    pub changed: u32,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MerchantUpdated {
    #[topic]
    pub owner: Address,
    pub settlement: Address,
    pub accepted_assets: Vec<Address>,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MerchantRejected {
    #[topic]
    pub owner: Address,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MerchantDeregistered {
    #[topic]
    pub owner: Address,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PauseToggled {
    pub paused: bool,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebhookRegistered {
    #[topic]
    pub owner: Address,
    pub url: String,
}

// Build metadata embedded in the wasm (SEP-0055), so anyone can trace a
// deployed contract back to its source; see scripts/verify.sh.
soroban_sdk::contractmeta!(key = "source_repo", val = "github:StellarPOS-App/contracts");
soroban_sdk::contractmeta!(key = "home_domain", val = "github.com/StellarPOS-App");
soroban_sdk::contractmeta!(key = "desc", val = "StellarPOS merchant registry");

#[contract]
pub struct MerchantRegistry;

#[contractimpl]
impl MerchantRegistry {
    /// Initializes the merchant registry.
    ///
    /// # Security
    ///
    /// In production deployments, `admin` **must** be a multisig contract
    /// (such as the deployed `treasury` M-of-N multisig) or a Stellar M-of-N
    /// multisig account rather than a single-key EOA. See `SECURITY.md`.
    pub fn initialize(env: Env, admin: Address) -> Result<(), RegistryError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(RegistryError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Paused, &false);
        env.storage().instance().set(&DataKey::MerchantCount, &0u32);
        extend_instance(&env);
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Merchant self-service
    // ---------------------------------------------------------------------

    /// Registers the caller as a merchant in `Pending` status.
    pub fn register(
        env: Env,
        owner: Address,
        name: String,
        settlement: Address,
        accepted_assets: Vec<Address>,
    ) -> Result<(), RegistryError> {
        Self::require_not_paused(&env)?;
        owner.require_auth();

        let key = DataKey::Merchant(owner.clone());
        if env.storage().persistent().has(&key) {
            return Err(RegistryError::AlreadyRegistered);
        }
        Self::validate_name(&name)?;
        Self::validate_assets(&accepted_assets)?;

        let now = env.ledger().timestamp();
        let merchant = Merchant {
            owner: owner.clone(),
            name: name.clone(),
            settlement: settlement.clone(),
            accepted_assets,
            status: MerchantStatus::Pending,
            registered_at: now,
            updated_at: now,
        };
        env.storage().persistent().set(&key, &merchant);
        extend_persistent(&env, &key);

        let count: u32 = env
            .storage()
            .instance()
            .get(&DataKey::MerchantCount)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::MerchantCount, &(count + 1));
        extend_instance(&env);

        MerchantRegistered {
            owner,
            name,
            settlement,
        }
        .publish(&env);
        Ok(())
    }

    /// Updates the settlement account and accepted assets in one call.
    pub fn update_profile(
        env: Env,
        owner: Address,
        settlement: Address,
        accepted_assets: Vec<Address>,
    ) -> Result<(), RegistryError> {
        Self::require_not_paused(&env)?;
        owner.require_auth();
        Self::validate_assets(&accepted_assets)?;

        let mut merchant = Self::load(&env, &owner)?;
        merchant.settlement = settlement.clone();
        merchant.accepted_assets = accepted_assets.clone();
        merchant.updated_at = env.ledger().timestamp();
        Self::save(&env, &merchant);

        MerchantUpdated {
            owner,
            settlement,
            accepted_assets,
        }
        .publish(&env);
        Ok(())
    }

    pub fn register_webhook(env: Env, owner: Address, url: String) -> Result<(), RegistryError> {
        Self::require_not_paused(&env)?;
        owner.require_auth();
        WebhookRegistered { owner, url }.publish(&env);
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Admin
    // ---------------------------------------------------------------------

    pub fn approve(env: Env, owner: Address) -> Result<(), RegistryError> {
        Self::set_status(&env, owner, MerchantStatus::Pending, MerchantStatus::Active)
    }

    /// Approves a batch of pending merchants in one transaction, e.g. after a
    /// review session. Owners that are unknown or not pending are skipped,
    /// so one stale entry does not block the rest. Returns how many were
    /// approved.
    pub fn approve_many(env: Env, owners: Vec<Address>) -> Result<u32, RegistryError> {
        if owners.len() > MAX_BATCH {
            return Err(RegistryError::BatchTooLarge);
        }
        Self::admin(&env)?.require_auth();
        let mut approved = 0;
        for owner in owners.iter() {
            if Self::apply_status_silent(
                &env,
                owner,
                MerchantStatus::Pending,
                MerchantStatus::Active,
            )
            .is_ok()
            {
                approved += 1;
            }
        }
        MerchantBatchStatusChanged {
            requested: owners.len(),
            changed: approved,
        }
        .publish(&env);
        Ok(approved)
    }

    /// Declines a pending registration and deletes it, so the applicant can
    /// fix their details and register again. Active or suspended merchants
    /// cannot be rejected; suspend them instead, which keeps their history.
    pub fn reject(env: Env, owner: Address) -> Result<(), RegistryError> {
        Self::admin(&env)?.require_auth();
        let merchant = Self::load(&env, &owner)?;
        if merchant.status != MerchantStatus::Pending {
            return Err(RegistryError::InvalidStatusTransition);
        }
        env.storage()
            .persistent()
            .remove(&DataKey::Merchant(owner.clone()));
        let count: u32 = env
            .storage()
            .instance()
            .get(&DataKey::MerchantCount)
            .unwrap_or(1);
        env.storage()
            .instance()
            .set(&DataKey::MerchantCount, &count.saturating_sub(1));
        extend_instance(&env);
        MerchantRejected { owner }.publish(&env);
        Ok(())
    }

    pub fn suspend(env: Env, owner: Address) -> Result<(), RegistryError> {
        Self::set_status(
            &env,
            owner,
            MerchantStatus::Active,
            MerchantStatus::Suspended,
        )
    }

    pub fn reinstate(env: Env, owner: Address) -> Result<(), RegistryError> {
        Self::set_status(
            &env,
            owner,
            MerchantStatus::Suspended,
            MerchantStatus::Active,
        )
    }

    /// Permanently removes a merchant from checkout and asks the configured
    /// escrow service to return every still-active escrow to its buyer.
    pub fn deregister(env: Env, owner: Address) -> Result<(), RegistryError> {
        Self::admin(&env)?.require_auth();
        let merchant = Self::load(&env, &owner)?;
        if !matches!(
            merchant.status,
            MerchantStatus::Active | MerchantStatus::Suspended
        ) {
            return Err(RegistryError::InvalidStatusTransition);
        }
        Self::apply_status_unchecked(&env, owner.clone(), MerchantStatus::Deregistered)?;
        if let Some(escrow) = env.storage().instance().get(&DataKey::EscrowService) {
            EscrowClient::new(&env, &escrow).cancel_for_merchant(&owner);
        }
        MerchantDeregistered { owner }.publish(&env);
        Ok(())
    }

    /// Links the escrow service used by `deregister`. It is intentionally an
    /// explicit deployment step so existing deployments remain compatible.
    pub fn set_escrow_service(env: Env, escrow: Option<Address>) -> Result<(), RegistryError> {
        Self::admin(&env)?.require_auth();
        match escrow {
            Some(address) => env
                .storage()
                .instance()
                .set(&DataKey::EscrowService, &address),
            None => env.storage().instance().remove(&DataKey::EscrowService),
        }
        extend_instance(&env);
        Ok(())
    }

    pub fn pause(env: Env) -> Result<(), RegistryError> {
        Self::set_paused(&env, true)
    }

    pub fn unpause(env: Env) -> Result<(), RegistryError> {
        Self::set_paused(&env, false)
    }

    pub fn transfer_admin(env: Env, new_admin: Address) -> Result<(), RegistryError> {
        Self::admin(&env)?.require_auth();
        new_admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        extend_instance(&env);
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Views (consumed by the payment processor and the backend)
    // ---------------------------------------------------------------------

    pub fn get_merchant(env: Env, owner: Address) -> Result<Merchant, RegistryError> {
        Self::load(&env, &owner)
    }

    /// True when the merchant exists and is `Active`.
    pub fn is_active(env: Env, owner: Address) -> bool {
        matches!(
            Self::load(&env, &owner).map(|m| m.status),
            Ok(MerchantStatus::Active)
        )
    }

    /// True when the merchant is active and lists `asset`.
    pub fn accepts(env: Env, owner: Address, asset: Address) -> bool {
        match Self::load(&env, &owner) {
            Ok(m) => m.status == MerchantStatus::Active && m.accepted_assets.contains(&asset),
            Err(_) => false,
        }
    }

    /// Everything checkout needs in one call: the settlement account if the
    /// merchant is active and accepts `asset`, otherwise `None`. Replaces an
    /// `accepts` + `settlement_of` pair, halving cross-contract calls and
    /// merchant reads per payment.
    pub fn checkout_target(env: Env, owner: Address, asset: Address) -> Option<Address> {
        let merchant = Self::load(&env, &owner).ok()?;
        (merchant.status == MerchantStatus::Active && merchant.accepted_assets.contains(&asset))
            .then_some(merchant.settlement)
    }

    /// Settlement account for an active merchant.
    pub fn settlement_of(env: Env, owner: Address) -> Result<Address, RegistryError> {
        let merchant = Self::load(&env, &owner)?;
        if merchant.status != MerchantStatus::Active {
            return Err(RegistryError::InvalidStatusTransition);
        }
        Ok(merchant.settlement)
    }

    pub fn merchant_count(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::MerchantCount)
            .unwrap_or(0)
    }

    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    pub fn get_admin(env: Env) -> Result<Address, RegistryError> {
        Self::admin(&env)
    }

    // ---------------------------------------------------------------------
    // Internals
    // ---------------------------------------------------------------------

    fn admin(env: &Env) -> Result<Address, RegistryError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(RegistryError::NotInitialized)
    }

    fn require_not_paused(env: &Env) -> Result<(), RegistryError> {
        if Self::is_paused(env.clone()) {
            return Err(RegistryError::Paused);
        }
        Ok(())
    }

    fn set_paused(env: &Env, paused: bool) -> Result<(), RegistryError> {
        Self::admin(env)?.require_auth();
        env.storage().instance().set(&DataKey::Paused, &paused);
        extend_instance(env);
        PauseToggled { paused }.publish(env);
        Ok(())
    }

    fn set_status(
        env: &Env,
        owner: Address,
        from: MerchantStatus,
        to: MerchantStatus,
    ) -> Result<(), RegistryError> {
        Self::admin(env)?.require_auth();
        Self::apply_status(env, owner, from, to)
    }

    /// Status change without the auth check, for callers that already
    /// authorized the admin once (batch operations).
    fn apply_status(
        env: &Env,
        owner: Address,
        from: MerchantStatus,
        to: MerchantStatus,
    ) -> Result<(), RegistryError> {
        let mut merchant = Self::load(env, &owner)?;
        if merchant.status != from {
            return Err(RegistryError::InvalidStatusTransition);
        }
        merchant.status = to;
        merchant.updated_at = env.ledger().timestamp();
        Self::save(env, &merchant);
        MerchantStatusChanged { owner, status: to }.publish(env);
        Ok(())
    }

    fn apply_status_unchecked(
        env: &Env,
        owner: Address,
        to: MerchantStatus,
    ) -> Result<(), RegistryError> {
        let mut merchant = Self::load(env, &owner)?;
        merchant.status = to;
        merchant.updated_at = env.ledger().timestamp();
        Self::save(env, &merchant);
        MerchantStatusChanged { owner, status: to }.publish(env);
        Ok(())
    }

    fn load(env: &Env, owner: &Address) -> Result<Merchant, RegistryError> {
        let key = DataKey::Merchant(owner.clone());
        let merchant = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(RegistryError::MerchantNotFound)?;
        extend_persistent(env, &key);
        Ok(merchant)
    }

    fn save(env: &Env, merchant: &Merchant) {
        let key = DataKey::Merchant(merchant.owner.clone());
        env.storage().persistent().set(&key, merchant);
        extend_persistent(env, &key);
    }

    fn validate_name(name: &String) -> Result<(), RegistryError> {
        if name.is_empty() || name.len() > MAX_NAME_LEN {
            return Err(RegistryError::InvalidName);
        }
        Ok(())
    }

    fn validate_assets(assets: &Vec<Address>) -> Result<(), RegistryError> {
        if assets.len() > MAX_ACCEPTED_ASSETS {
            return Err(RegistryError::TooManyAssets);
        }
        for (i, asset) in assets.iter().enumerate() {
            if let Some(first) = assets.first_index_of(&asset) {
                if first as usize != i {
                    return Err(RegistryError::DuplicateAsset);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod test;
