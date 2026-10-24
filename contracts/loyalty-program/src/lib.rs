//! Loyalty program.
//!
//! Each merchant runs its own points program. Points accrue on purchases,
//! either automatically when the payment processor settles a sale, or
//! manually by the merchant for off-chain sales (cash, card). Customers
//! redeem points with the merchant for discounts.
//!
//! Tiers are automatic: a customer's tier is derived from the *lifetime*
//! points they have earned with a merchant (redeeming never demotes them),
//! and higher tiers earn at a multiplier.
//!
//! | Tier     | Lifetime points | Earn multiplier |
//! |----------|-----------------|-----------------|
//! | Bronze   | 0               | 1.00x           |
//! | Silver   | 1,000           | 1.10x           |
//! | Gold     | 5,000           | 1.25x           |
//! | Platinum | 20,000          | 1.50x           |

#![no_std]

use shared::{extend_instance, extend_persistent, BPS_DENOMINATOR};
use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, Address, Env, Vec,
};

/// Most payment processors allowed to accrue at once (e.g. old and new
/// during an upgrade).
pub const MAX_PROCESSORS: u32 = 5;

pub const SILVER_THRESHOLD: u64 = 1_000;
pub const GOLD_THRESHOLD: u64 = 5_000;
pub const PLATINUM_THRESHOLD: u64 = 20_000;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum LoyaltyError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Paused = 3,
    NotEnrolled = 4,
    AlreadyEnrolled = 5,
    InvalidRate = 6,
    InvalidAmount = 7,
    InsufficientPoints = 8,
    Unauthorized = 9,
    Overflow = 10,
    TooManyProcessors = 11,
    ProcessorNotFound = 12,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum Tier {
    Bronze,
    Silver,
    Gold,
    Platinum,
}

impl Tier {
    pub fn for_lifetime(lifetime: u64) -> Tier {
        if lifetime >= PLATINUM_THRESHOLD {
            Tier::Platinum
        } else if lifetime >= GOLD_THRESHOLD {
            Tier::Gold
        } else if lifetime >= SILVER_THRESHOLD {
            Tier::Silver
        } else {
            Tier::Bronze
        }
    }

    pub fn multiplier_bps(self) -> u32 {
        match self {
            Tier::Bronze => 10_000,
            Tier::Silver => 11_000,
            Tier::Gold => 12_500,
            Tier::Platinum => 15_000,
        }
    }
}

/// A merchant's program configuration.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Program {
    /// Points earned per whole unit of the sale asset at Bronze tier.
    pub points_per_unit: u32,
    /// Smallest-unit amount that counts as one whole unit
    /// (10_000_000 for 7-decimal Stellar assets).
    pub unit: i128,
    pub active: bool,
}

#[contracttype]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Account {
    pub points: u64,
    pub lifetime: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccountView {
    pub points: u64,
    pub lifetime: u64,
    pub tier: Tier,
    /// Lifetime points still needed for the next tier; 0 at Platinum.
    pub to_next_tier: u64,
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Admin,
    /// Single processor written by versions before the allowlist; still
    /// honored so upgraded deployments keep working.
    Processor,
    Processors,
    Paused,
    Program(Address),
    Account(Address, Address),
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PointsEarned {
    #[topic]
    pub merchant: Address,
    #[topic]
    pub customer: Address,
    pub points: u64,
    pub tier: Tier,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PointsRedeemed {
    #[topic]
    pub merchant: Address,
    #[topic]
    pub customer: Address,
    pub points: u64,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessorAccess {
    #[topic]
    pub processor: Address,
    pub allowed: bool,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TierChanged {
    #[topic]
    pub merchant: Address,
    #[topic]
    pub customer: Address,
    pub tier: Tier,
}

/// Points earned for `amount` at `tier`: whole units times the program rate,
/// boosted by the tier multiplier. `None` on overflow.
fn points_for(program: &Program, tier: Tier, amount: i128) -> Option<u64> {
    let base = amount.checked_mul(program.points_per_unit as i128)? / program.unit;
    let boosted = base.checked_mul(tier.multiplier_bps() as i128)? / BPS_DENOMINATOR as i128;
    u64::try_from(boosted).ok()
}

// Build metadata embedded in the wasm (SEP-0055), so anyone can trace a
// deployed contract back to its source; see scripts/verify.sh.
soroban_sdk::contractmeta!(key = "source_repo", val = "github:StellarPOS-App/contracts");
soroban_sdk::contractmeta!(key = "home_domain", val = "github.com/StellarPOS-App");
soroban_sdk::contractmeta!(key = "desc", val = "StellarPOS loyalty program");

#[contract]
pub struct LoyaltyProgram;

#[contractimpl]
impl LoyaltyProgram {
    pub fn initialize(env: Env, admin: Address) -> Result<(), LoyaltyError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(LoyaltyError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Paused, &false);
        extend_instance(&env);
        Ok(())
    }

    /// Allows a payment processor contract to accrue points on settled
    /// sales. Several can be allowed at once, so a new processor can go live
    /// before the old one is retired. Adding one twice is a no-op.
    pub fn add_processor(env: Env, processor: Address) -> Result<(), LoyaltyError> {
        Self::admin(&env)?.require_auth();
        let mut list = Self::processors(env.clone());
        if list.contains(&processor) {
            return Ok(());
        }
        if list.len() >= MAX_PROCESSORS {
            return Err(LoyaltyError::TooManyProcessors);
        }
        list.push_back(processor.clone());
        Self::save_processors(&env, &list);
        ProcessorAccess {
            processor,
            allowed: true,
        }
        .publish(&env);
        Ok(())
    }

    /// Revokes a processor's access immediately, e.g. after retiring it or
    /// if it is compromised.
    pub fn remove_processor(env: Env, processor: Address) -> Result<(), LoyaltyError> {
        Self::admin(&env)?.require_auth();
        let mut list = Self::processors(env.clone());
        let index = list
            .first_index_of(&processor)
            .ok_or(LoyaltyError::ProcessorNotFound)?;
        list.remove(index);
        Self::save_processors(&env, &list);
        ProcessorAccess {
            processor,
            allowed: false,
        }
        .publish(&env);
        Ok(())
    }

    /// Backwards-compatible name for add_processor.
    pub fn set_processor(env: Env, processor: Address) -> Result<(), LoyaltyError> {
        Self::add_processor(env, processor)
    }

    /// Processors currently allowed to accrue points.
    pub fn processors(env: Env) -> Vec<Address> {
        let mut list: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::Processors)
            .unwrap_or_else(|| Vec::new(&env));
        if let Some(legacy) = env
            .storage()
            .instance()
            .get::<_, Address>(&DataKey::Processor)
        {
            if !list.contains(&legacy) {
                list.push_front(legacy);
            }
        }
        list
    }

    pub fn pause(env: Env) -> Result<(), LoyaltyError> {
        Self::admin(&env)?.require_auth();
        env.storage().instance().set(&DataKey::Paused, &true);
        Ok(())
    }

    pub fn unpause(env: Env) -> Result<(), LoyaltyError> {
        Self::admin(&env)?.require_auth();
        env.storage().instance().set(&DataKey::Paused, &false);
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Merchant program management
    // ---------------------------------------------------------------------

    pub fn enroll(
        env: Env,
        merchant: Address,
        points_per_unit: u32,
        unit: i128,
    ) -> Result<(), LoyaltyError> {
        Self::require_not_paused(&env)?;
        merchant.require_auth();
        let key = DataKey::Program(merchant);
        if env.storage().persistent().has(&key) {
            return Err(LoyaltyError::AlreadyEnrolled);
        }
        Self::validate_rate(points_per_unit, unit)?;
        let program = Program {
            points_per_unit,
            unit,
            active: true,
        };
        env.storage().persistent().set(&key, &program);
        extend_persistent(&env, &key);
        Ok(())
    }

    pub fn configure(
        env: Env,
        merchant: Address,
        points_per_unit: u32,
        unit: i128,
        active: bool,
    ) -> Result<(), LoyaltyError> {
        merchant.require_auth();
        Self::validate_rate(points_per_unit, unit)?;
        let key = DataKey::Program(merchant.clone());
        Self::program(&env, &merchant)?;
        env.storage().persistent().set(
            &key,
            &Program {
                points_per_unit,
                unit,
                active,
            },
        );
        extend_persistent(&env, &key);
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Earning and redeeming
    // ---------------------------------------------------------------------

    /// Credits points for a purchase of `amount` (smallest units).
    ///
    /// `caller` must be either the registered payment processor contract or
    /// the merchant itself. Returns the points credited; returns 0 without
    /// failing when the merchant has no active program, so a processor can
    /// call this unconditionally on every sale.
    pub fn accrue(
        env: Env,
        caller: Address,
        merchant: Address,
        customer: Address,
        amount: i128,
    ) -> Result<u64, LoyaltyError> {
        Self::require_not_paused(&env)?;
        caller.require_auth();
        if caller != merchant && !Self::processors(env.clone()).contains(&caller) {
            return Err(LoyaltyError::Unauthorized);
        }
        if amount <= 0 {
            return Err(LoyaltyError::InvalidAmount);
        }

        let program = match Self::program(&env, &merchant) {
            Ok(p) if p.active => p,
            _ => return Ok(0),
        };

        let key = DataKey::Account(merchant.clone(), customer.clone());
        let mut account: Account = env.storage().persistent().get(&key).unwrap_or_default();
        let tier_before = Tier::for_lifetime(account.lifetime);

        let points = points_for(&program, tier_before, amount).ok_or(LoyaltyError::Overflow)?;
        if points == 0 {
            return Ok(0);
        }

        account.points = account
            .points
            .checked_add(points)
            .ok_or(LoyaltyError::Overflow)?;
        account.lifetime = account
            .lifetime
            .checked_add(points)
            .ok_or(LoyaltyError::Overflow)?;
        env.storage().persistent().set(&key, &account);
        extend_persistent(&env, &key);

        let tier_after = Tier::for_lifetime(account.lifetime);
        PointsEarned {
            merchant: merchant.clone(),
            customer: customer.clone(),
            points,
            tier: tier_after,
        }
        .publish(&env);
        if tier_after != tier_before {
            TierChanged {
                merchant,
                customer,
                tier: tier_after,
            }
            .publish(&env);
        }
        Ok(points)
    }

    /// Spends `points` from the customer's balance with `merchant`. Both the
    /// customer and the merchant sign: the customer consents to spending, the
    /// merchant commits to honoring the discount.
    pub fn redeem(
        env: Env,
        merchant: Address,
        customer: Address,
        points: u64,
    ) -> Result<u64, LoyaltyError> {
        Self::require_not_paused(&env)?;
        customer.require_auth();
        merchant.require_auth();
        if points == 0 {
            return Err(LoyaltyError::InvalidAmount);
        }
        Self::program(&env, &merchant)?;

        let key = DataKey::Account(merchant.clone(), customer.clone());
        let mut account: Account = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(LoyaltyError::InsufficientPoints)?;
        if account.points < points {
            return Err(LoyaltyError::InsufficientPoints);
        }
        account.points -= points;
        env.storage().persistent().set(&key, &account);
        extend_persistent(&env, &key);

        PointsRedeemed {
            merchant,
            customer,
            points,
        }
        .publish(&env);
        Ok(account.points)
    }

    // ---------------------------------------------------------------------
    // Views
    // ---------------------------------------------------------------------

    pub fn get_program(env: Env, merchant: Address) -> Result<Program, LoyaltyError> {
        Self::program(&env, &merchant)
    }

    pub fn get_account(env: Env, merchant: Address, customer: Address) -> AccountView {
        let account: Account = env
            .storage()
            .persistent()
            .get(&DataKey::Account(merchant, customer))
            .unwrap_or_default();
        let tier = Tier::for_lifetime(account.lifetime);
        let next = match tier {
            Tier::Bronze => SILVER_THRESHOLD,
            Tier::Silver => GOLD_THRESHOLD,
            Tier::Gold => PLATINUM_THRESHOLD,
            Tier::Platinum => account.lifetime,
        };
        AccountView {
            points: account.points,
            lifetime: account.lifetime,
            tier,
            to_next_tier: next - account.lifetime,
        }
    }

    /// Previews how many points a purchase would earn without writing state.
    pub fn quote(env: Env, merchant: Address, customer: Address, amount: i128) -> u64 {
        let program = match Self::program(&env, &merchant) {
            Ok(p) if p.active && amount > 0 => p,
            _ => return 0,
        };
        let tier = Self::get_account(env, merchant, customer).tier;
        points_for(&program, tier, amount).unwrap_or(0)
    }

    // ---------------------------------------------------------------------
    // Internals
    // ---------------------------------------------------------------------

    fn admin(env: &Env) -> Result<Address, LoyaltyError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(LoyaltyError::NotInitialized)
    }

    /// Stores the allowlist and retires the legacy single-processor key.
    fn save_processors(env: &Env, list: &Vec<Address>) {
        env.storage().instance().set(&DataKey::Processors, list);
        env.storage().instance().remove(&DataKey::Processor);
        extend_instance(env);
    }

    fn require_not_paused(env: &Env) -> Result<(), LoyaltyError> {
        let paused: bool = env
            .storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false);
        if paused {
            return Err(LoyaltyError::Paused);
        }
        Ok(())
    }

    fn program(env: &Env, merchant: &Address) -> Result<Program, LoyaltyError> {
        let key = DataKey::Program(merchant.clone());
        let program = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(LoyaltyError::NotEnrolled)?;
        extend_persistent(env, &key);
        Ok(program)
    }

    fn validate_rate(points_per_unit: u32, unit: i128) -> Result<(), LoyaltyError> {
        if points_per_unit == 0 || points_per_unit > 1_000 || unit <= 0 {
            return Err(LoyaltyError::InvalidRate);
        }
        Ok(())
    }
}

#[cfg(test)]
mod test;
