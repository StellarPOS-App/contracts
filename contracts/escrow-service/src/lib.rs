//! Escrow service.
//!
//! Holds a buyer's payment for orders that are not fulfilled at the counter
//! (deliveries, pre-orders, custom work) until the buyer confirms receipt.
//!
//! ```text
//!            create (buyer funds)
//!                    |
//!                 Funded --release / claim after release_at--> Released
//!                 |    \--refund by seller--------------------> Refunded
//!        raise_dispute (before release_at)
//!                    |
//!                Disputed --arbiter resolves before deadline--> Resolved
//!                         \--deadline passes, default rule----> Resolved
//! ```
//!
//! * The buyer can release funds early, in full or in parts.
//! * After `release_at`, the seller can claim whatever is left, unless a
//!   dispute was raised first. This is the auto-release that protects
//!   sellers from buyers who never confirm.
//! * Disputes freeze the escrow. The arbiter splits the remaining funds by
//!   basis points. If the arbiter misses `arbitration_window`, anyone can
//!   apply the default ruling for the dispute reason (see [`dispute`]).

#![no_std]

pub mod dispute;

use dispute::DisputeReason;
use shared::{bps_of, extend_instance, extend_persistent};
use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, token, Address, BytesN,
    Env, Vec,
};

pub const MAX_EVIDENCE: u32 = 10;
pub const MIN_ESCROW_LOCK_PERIOD: u64 = 60 * 60;
pub const MIN_ARBITRATION_WINDOW: u64 = 60 * 60;
pub const MAX_ARBITRATION_WINDOW: u64 = 30 * 24 * 60 * 60;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum EscrowError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Paused = 3,
    InvalidAmount = 4,
    InvalidParties = 5,
    InvalidSchedule = 6,
    EscrowNotFound = 7,
    InvalidStatus = 8,
    NotParty = 9,
    NotArbiter = 10,
    ReleaseNotDue = 11,
    DisputeWindowClosed = 12,
    ReasonNotAllowed = 13,
    TooMuchEvidence = 14,
    DeadlineNotPassed = 15,
    DeadlinePassed = 16,
    InvalidSplit = 17,
    Overflow = 18,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum EscrowStatus {
    Funded,
    Disputed,
    Released,
    Refunded,
    Resolved,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PartyRole {
    Buyer,
    Seller,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Dispute {
    pub raised_by: Address,
    pub reason: DisputeReason,
    pub raised_at: u64,
    /// Arbiter must rule by this time; afterwards the default applies.
    pub deadline: u64,
    /// Buyer share chosen by the arbiter or the default rule, once resolved.
    /// `UNRESOLVED` while the dispute is still open.
    pub buyer_bps: u32,
    pub by_default: bool,
}

/// Sentinel for "arbiter has not ruled yet".
const UNRESOLVED: u32 = u32::MAX;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Escrow {
    pub id: u64,
    pub buyer: Address,
    pub seller: Address,
    pub arbiter: Address,
    pub asset: Address,
    pub amount: i128,
    /// Paid out to the seller so far.
    pub released: i128,
    /// Returned to the buyer so far.
    pub refunded: i128,
    pub release_at: u64,
    pub arbitration_window: u64,
    pub status: EscrowStatus,
    pub created_at: u64,
}

impl Escrow {
    pub fn remaining(&self) -> i128 {
        self.amount - self.released - self.refunded
    }

    fn role_of(&self, who: &Address) -> Option<PartyRole> {
        if *who == self.buyer {
            Some(PartyRole::Buyer)
        } else if *who == self.seller {
            Some(PartyRole::Seller)
        } else {
            None
        }
    }
}

/// A hash of an off-chain evidence file (photo, tracking page, chat log).
/// Only the hash is stored; files stay off-chain.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Evidence {
    pub party: Address,
    pub hash: BytesN<32>,
    pub submitted_at: u64,
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Admin,
    Paused,
    Count,
    Escrow(u64),
    Dispute(u64),
    Evidence(u64),
    MinLockPeriod,
    LockUntil(u64),
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EscrowCreated {
    #[topic]
    pub id: u64,
    pub buyer: Address,
    pub seller: Address,
    pub amount: i128,
    pub release_at: u64,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FundsReleased {
    #[topic]
    pub id: u64,
    pub to_seller: i128,
    pub to_buyer: i128,
    pub status: EscrowStatus,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisputeRaised {
    #[topic]
    pub id: u64,
    pub raised_by: Address,
    pub reason: DisputeReason,
    pub deadline: u64,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvidenceSubmitted {
    #[topic]
    pub id: u64,
    pub party: Address,
    pub hash: BytesN<32>,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisputeResolved {
    #[topic]
    pub id: u64,
    pub buyer_bps: u32,
    pub by_default: bool,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EscrowLockPeriodChanged {
    pub seconds: u64,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectPayment {
    #[topic]
    pub buyer: Address,
    #[topic]
    pub seller: Address,
    pub asset: Address,
    pub amount: i128,
}

// Build metadata embedded in the wasm (SEP-0055), so anyone can trace a
// deployed contract back to its source; see scripts/verify.sh.
soroban_sdk::contractmeta!(key = "source_repo", val = "github:StellarPOS-App/contracts");
soroban_sdk::contractmeta!(key = "home_domain", val = "github.com/StellarPOS-App");
soroban_sdk::contractmeta!(key = "desc", val = "StellarPOS escrow service");

#[contract]
pub struct EscrowService;

#[contractimpl]
impl EscrowService {
    pub fn initialize(env: Env, admin: Address) -> Result<(), EscrowError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(EscrowError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Paused, &false);
        env.storage().instance().set(&DataKey::Count, &0u64);
        extend_instance(&env);
        Ok(())
    }

    /// Fast path for immediate direct payments when both buyer and seller
    /// agree to bypass escrow creation, storage writes, and separate release.
    pub fn pay_direct(
        env: Env,
        buyer: Address,
        seller: Address,
        asset: Address,
        amount: i128,
    ) -> Result<(), EscrowError> {
        Self::require_not_paused(&env)?;
        if amount <= 0 {
            return Err(EscrowError::InvalidAmount);
        }
        if buyer == seller {
            return Err(EscrowError::InvalidParties);
        }
        buyer.require_auth();
        seller.require_auth();

        token::Client::new(&env, &asset).transfer(&buyer, &seller, &amount);

        DirectPayment {
            buyer,
            seller,
            asset,
            amount,
        }
        .publish(&env);
        Ok(())
    }

    /// Opens an escrow and pulls `amount` from the buyer into the contract.
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        env: Env,
        buyer: Address,
        seller: Address,
        arbiter: Address,
        asset: Address,
        amount: i128,
        release_at: u64,
        arbitration_window: u64,
    ) -> Result<u64, EscrowError> {
        Self::require_not_paused(&env)?;
        buyer.require_auth();

        if amount <= 0 {
            return Err(EscrowError::InvalidAmount);
        }
        if buyer == seller || arbiter == buyer || arbiter == seller {
            return Err(EscrowError::InvalidParties);
        }
        let now = env.ledger().timestamp();
        // Store the promised lock at funding so future policy changes only
        // affect new escrows.
        let lock_until = now
            .checked_add(Self::min_lock_period(env.clone()))
            .ok_or(EscrowError::Overflow)?;
        if release_at < lock_until
            || !(MIN_ARBITRATION_WINDOW..=MAX_ARBITRATION_WINDOW).contains(&arbitration_window)
        {
            return Err(EscrowError::InvalidSchedule);
        }

        token::Client::new(&env, &asset).transfer(&buyer, env.current_contract_address(), &amount);

        let id: u64 = env.storage().instance().get(&DataKey::Count).unwrap_or(0) + 1;
        let escrow = Escrow {
            id,
            buyer: buyer.clone(),
            seller: seller.clone(),
            arbiter,
            asset,
            amount,
            released: 0,
            refunded: 0,
            release_at,
            arbitration_window,
            status: EscrowStatus::Funded,
            created_at: now,
        };
        Self::save(&env, &escrow);
        let lock_key = DataKey::LockUntil(id);
        env.storage().persistent().set(&lock_key, &lock_until);
        extend_persistent(&env, &lock_key);
        env.storage().instance().set(&DataKey::Count, &id);
        extend_instance(&env);

        EscrowCreated {
            id,
            buyer,
            seller,
            amount,
            release_at,
        }
        .publish(&env);
        Ok(id)
    }

    /// Buyer confirms delivery and releases everything left to the seller.
    pub fn release(env: Env, id: u64) -> Result<(), EscrowError> {
        Self::require_not_paused(&env)?;
        let mut escrow = Self::funded(&env, id)?;
        escrow.buyer.require_auth();
        // Buyer approval still leaves the minimum dispute window intact.
        Self::require_release_unlocked(&env, &escrow)?;
        let amount = escrow.remaining();
        Self::payout(&env, &mut escrow, amount, 0, EscrowStatus::Released);
        Ok(())
    }

    /// Buyer releases part of the escrow, e.g. per delivered milestone.
    pub fn release_partial(env: Env, id: u64, amount: i128) -> Result<i128, EscrowError> {
        Self::require_not_paused(&env)?;
        let mut escrow = Self::funded(&env, id)?;
        escrow.buyer.require_auth();
        Self::require_release_unlocked(&env, &escrow)?;
        if amount <= 0 || amount > escrow.remaining() {
            return Err(EscrowError::InvalidAmount);
        }
        // A partial payout keeps the remaining balance open for disputes.
        let status = if amount == escrow.remaining() {
            EscrowStatus::Released
        } else {
            EscrowStatus::Funded
        };
        Self::payout(&env, &mut escrow, amount, 0, status);
        Ok(escrow.remaining())
    }

    /// Seller claims the remainder once `release_at` has passed without a
    /// dispute.
    pub fn claim(env: Env, id: u64) -> Result<(), EscrowError> {
        Self::require_not_paused(&env)?;
        let mut escrow = Self::funded(&env, id)?;
        escrow.seller.require_auth();
        Self::require_release_unlocked(&env, &escrow)?;
        if env.ledger().timestamp() < escrow.release_at {
            return Err(EscrowError::ReleaseNotDue);
        }
        let amount = escrow.remaining();
        Self::payout(&env, &mut escrow, amount, 0, EscrowStatus::Released);
        Ok(())
    }

    /// Seller cancels the order and returns the remainder to the buyer.
    /// Allowed even while paused: returning funds is always safe.
    pub fn refund(env: Env, id: u64) -> Result<(), EscrowError> {
        let mut escrow = Self::funded(&env, id)?;
        escrow.seller.require_auth();
        let amount = escrow.remaining();
        Self::payout(&env, &mut escrow, 0, amount, EscrowStatus::Refunded);
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Disputes
    // ---------------------------------------------------------------------

    pub fn raise_dispute(
        env: Env,
        party: Address,
        id: u64,
        reason: DisputeReason,
        evidence: Option<BytesN<32>>,
    ) -> Result<u64, EscrowError> {
        Self::require_not_paused(&env)?;
        party.require_auth();
        let mut escrow = Self::funded(&env, id)?;
        let role = escrow.role_of(&party).ok_or(EscrowError::NotParty)?;
        if !reason.can_be_raised_by(role) {
            return Err(EscrowError::ReasonNotAllowed);
        }
        let now = env.ledger().timestamp();
        // Buyers must dispute before auto-release kicks in. Sellers may
        // dispute any time while funded, to get a ruling sooner than
        // release_at (e.g. a buyer blocking a partial release).
        let window_open = match role {
            PartyRole::Buyer => now < escrow.release_at,
            PartyRole::Seller => true,
        };
        if !window_open {
            return Err(EscrowError::DisputeWindowClosed);
        }

        // Allow the arbiter to rule at the unlock time even when the
        // arbitration window would otherwise end sooner.
        let deadline = now
            .checked_add(escrow.arbitration_window)
            .ok_or(EscrowError::Overflow)?
            .max(Self::lock_until(&env, &escrow));
        escrow.status = EscrowStatus::Disputed;
        Self::save(&env, &escrow);
        Self::save_dispute(
            &env,
            id,
            &Dispute {
                raised_by: party.clone(),
                reason,
                raised_at: now,
                deadline,
                buyer_bps: UNRESOLVED,
                by_default: false,
            },
        );
        if let Some(hash) = evidence {
            Self::push_evidence(&env, id, &party, hash)?;
        }

        DisputeRaised {
            id,
            raised_by: party,
            reason,
            deadline,
        }
        .publish(&env);
        Ok(deadline)
    }

    pub fn submit_evidence(
        env: Env,
        party: Address,
        id: u64,
        hash: BytesN<32>,
    ) -> Result<u32, EscrowError> {
        party.require_auth();
        let escrow = Self::load(&env, id)?;
        if escrow.status != EscrowStatus::Disputed {
            return Err(EscrowError::InvalidStatus);
        }
        escrow.role_of(&party).ok_or(EscrowError::NotParty)?;
        Self::push_evidence(&env, id, &party, hash)
    }

    /// Arbiter splits the remaining funds: `buyer_bps` of it back to the
    /// buyer, the rest to the seller.
    pub fn resolve(env: Env, id: u64, buyer_bps: u32) -> Result<(), EscrowError> {
        let escrow = Self::disputed(&env, id)?;
        escrow.arbiter.require_auth();
        let dispute = Self::load_dispute(&env, id)?;
        // The arbiter has the deadline second; the default starts after it.
        if env.ledger().timestamp() > dispute.deadline {
            return Err(EscrowError::DeadlinePassed);
        }
        Self::settle_dispute(&env, escrow, dispute, buyer_bps, false)
    }

    /// Applies the default ruling once the arbiter has missed the deadline.
    /// Callable by anyone, so funds can never be stranded.
    pub fn resolve_by_default(env: Env, id: u64) -> Result<u32, EscrowError> {
        let escrow = Self::disputed(&env, id)?;
        let dispute = Self::load_dispute(&env, id)?;
        if env.ledger().timestamp() <= dispute.deadline {
            return Err(EscrowError::DeadlineNotPassed);
        }
        let bps = dispute.reason.default_buyer_bps();
        Self::settle_dispute(&env, escrow, dispute, bps, true)?;
        Ok(bps)
    }

    // ---------------------------------------------------------------------
    // Admin and views
    // ---------------------------------------------------------------------

    pub fn pause(env: Env) -> Result<(), EscrowError> {
        Self::admin(&env)?.require_auth();
        env.storage().instance().set(&DataKey::Paused, &true);
        Ok(())
    }

    pub fn unpause(env: Env) -> Result<(), EscrowError> {
        Self::admin(&env)?.require_auth();
        env.storage().instance().set(&DataKey::Paused, &false);
        Ok(())
    }

    /// Changes the minimum lock for future escrows. The admin must sign.
    pub fn set_min_lock_period(env: Env, seconds: u64) -> Result<(), EscrowError> {
        Self::admin(&env)?.require_auth();
        if seconds < MIN_ESCROW_LOCK_PERIOD {
            return Err(EscrowError::InvalidSchedule);
        }
        env.storage().instance().set(&DataKey::MinLockPeriod, &seconds);
        extend_instance(&env);
        EscrowLockPeriodChanged { seconds }.publish(&env);
        Ok(())
    }

    /// Returns the minimum funding-to-release delay for new escrows.
    pub fn min_lock_period(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::MinLockPeriod)
            .unwrap_or(MIN_ESCROW_LOCK_PERIOD)
    }

    pub fn get_escrow(env: Env, id: u64) -> Result<Escrow, EscrowError> {
        Self::load(&env, id)
    }

    pub fn get_dispute(env: Env, id: u64) -> Result<Dispute, EscrowError> {
        Self::load_dispute(&env, id)
    }

    pub fn get_evidence(env: Env, id: u64) -> Vec<Evidence> {
        env.storage()
            .persistent()
            .get(&DataKey::Evidence(id))
            .unwrap_or_else(|| Vec::new(&env))
    }

    pub fn escrow_count(env: Env) -> u64 {
        env.storage().instance().get(&DataKey::Count).unwrap_or(0)
    }

    // ---------------------------------------------------------------------
    // Internals
    // ---------------------------------------------------------------------

    fn admin(env: &Env) -> Result<Address, EscrowError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(EscrowError::NotInitialized)
    }

    fn require_not_paused(env: &Env) -> Result<(), EscrowError> {
        if env
            .storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
        {
            return Err(EscrowError::Paused);
        }
        Ok(())
    }

    fn lock_until(env: &Env, escrow: &Escrow) -> u64 {
        // Older records have no snapshot and use the protocol minimum.
        env.storage()
            .persistent()
            .get(&DataKey::LockUntil(escrow.id))
            .unwrap_or_else(|| escrow.created_at.saturating_add(MIN_ESCROW_LOCK_PERIOD))
    }

    fn require_release_unlocked(env: &Env, escrow: &Escrow) -> Result<(), EscrowError> {
        if env.ledger().timestamp() < Self::lock_until(env, escrow) {
            return Err(EscrowError::ReleaseNotDue);
        }
        Ok(())
    }

    fn settle_dispute(
        env: &Env,
        mut escrow: Escrow,
        mut dispute: Dispute,
        buyer_bps: u32,
        by_default: bool,
    ) -> Result<(), EscrowError> {
        // Split the held remainder rather than funds already paid out on
        // completed milestones. Assign any rounding remainder to the seller.
        let remaining = escrow.remaining();
        let to_buyer = bps_of(remaining, buyer_bps).ok_or(EscrowError::InvalidSplit)?;
        let to_seller = remaining - to_buyer;
        if to_seller > 0 {
            Self::require_release_unlocked(env, &escrow)?;
        }

        // Persist the ruling and payout together in the same invocation.
        dispute.buyer_bps = buyer_bps;
        dispute.by_default = by_default;
        Self::save_dispute(env, escrow.id, &dispute);
        Self::payout(
            env,
            &mut escrow,
            to_seller,
            to_buyer,
            EscrowStatus::Resolved,
        );
        DisputeResolved {
            id: escrow.id,
            buyer_bps,
            by_default,
        }
        .publish(env);
        Ok(())
    }

    /// Moves funds out of the contract and records the new state.
    fn payout(
        env: &Env,
        escrow: &mut Escrow,
        to_seller: i128,
        to_buyer: i128,
        status: EscrowStatus,
    ) {
        // Use the asset captured at funding for every payout, even if
        // another escrow is opened with a different asset.
        let token = token::Client::new(env, &escrow.asset);
        let this = env.current_contract_address();
        if to_seller > 0 {
            token.transfer(&this, &escrow.seller, &to_seller);
        }
        if to_buyer > 0 {
            token.transfer(&this, &escrow.buyer, &to_buyer);
        }
        escrow.released += to_seller;
        escrow.refunded += to_buyer;
        escrow.status = status;
        Self::save(env, escrow);
        if status != EscrowStatus::Funded {
            env.storage().persistent().remove(&DataKey::LockUntil(escrow.id));
        }
        FundsReleased {
            id: escrow.id,
            to_seller,
            to_buyer,
            status,
        }
        .publish(env);
    }

    fn push_evidence(
        env: &Env,
        id: u64,
        party: &Address,
        hash: BytesN<32>,
    ) -> Result<u32, EscrowError> {
        let key = DataKey::Evidence(id);
        let mut list: Vec<Evidence> = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(env));
        if list.len() >= MAX_EVIDENCE {
            return Err(EscrowError::TooMuchEvidence);
        }
        list.push_back(Evidence {
            party: party.clone(),
            hash: hash.clone(),
            submitted_at: env.ledger().timestamp(),
        });
        env.storage().persistent().set(&key, &list);
        extend_persistent(env, &key);
        EvidenceSubmitted {
            id,
            party: party.clone(),
            hash,
        }
        .publish(env);
        Ok(list.len())
    }

    fn funded(env: &Env, id: u64) -> Result<Escrow, EscrowError> {
        let escrow = Self::load(env, id)?;
        if escrow.status != EscrowStatus::Funded {
            return Err(EscrowError::InvalidStatus);
        }
        Ok(escrow)
    }

    fn disputed(env: &Env, id: u64) -> Result<Escrow, EscrowError> {
        let escrow = Self::load(env, id)?;
        if escrow.status != EscrowStatus::Disputed {
            return Err(EscrowError::InvalidStatus);
        }
        Ok(escrow)
    }

    fn load_dispute(env: &Env, id: u64) -> Result<Dispute, EscrowError> {
        env.storage()
            .persistent()
            .get(&DataKey::Dispute(id))
            .ok_or(EscrowError::InvalidStatus)
    }

    fn save_dispute(env: &Env, id: u64, dispute: &Dispute) {
        let key = DataKey::Dispute(id);
        env.storage().persistent().set(&key, dispute);
        extend_persistent(env, &key);
    }

    fn load(env: &Env, id: u64) -> Result<Escrow, EscrowError> {
        let key = DataKey::Escrow(id);
        let escrow = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(EscrowError::EscrowNotFound)?;
        extend_persistent(env, &key);
        let lock_key = DataKey::LockUntil(id);
        if env.storage().persistent().has(&lock_key) {
            extend_persistent(env, &lock_key);
        }
        Ok(escrow)
    }

    fn save(env: &Env, escrow: &Escrow) {
        let key = DataKey::Escrow(escrow.id);
        env.storage().persistent().set(&key, escrow);
        extend_persistent(env, &key);
    }
}

#[cfg(test)]
mod test;
