//! Payment processor.
//!
//! Settles a point-of-sale payment in a single atomic transaction:
//!
//! 1. Checks the merchant is active in the merchant registry and accepts
//!    the asset being paid.
//! 2. Rejects a second payment for the same `(merchant, order_ref)` so a
//!    double-tapped "Pay" button can never charge a customer twice.
//! 3. Moves the protocol fee to the treasury and the net amount to the
//!    merchant's settlement account, straight from the payer. The contract
//!    never holds customer funds.
//! 4. Optionally accrues loyalty points for the payer.
//!
//! Refunds are pushed by the merchant's settlement account (which received
//! the funds) back to the original payer, up to the gross amount paid.

#![no_std]

use shared::{extend_instance, extend_persistent, split_fee, MAX_FEE_BPS};
use soroban_sdk::{
    contract, contractclient, contracterror, contractevent, contractimpl, contracttype, token,
    Address, Env, Map, String, Vec,
};

pub const MAX_ORDER_REF_LEN: u32 = 64;
/// Most payments in one pay_batch call, to stay well inside resource limits.
pub const MAX_BATCH: u32 = 20;

/// The subset of the merchant registry this contract calls.
#[allow(dead_code)]
#[contractclient(name = "RegistryClient")]
pub trait RegistryInterface {
    fn checkout_target(env: Env, owner: Address, asset: Address) -> Option<Address>;
}

/// The subset of the loyalty program this contract calls.
#[allow(dead_code)]
#[contractclient(name = "LoyaltyClient")]
pub trait LoyaltyInterface {
    fn accrue(env: Env, caller: Address, merchant: Address, customer: Address, amount: i128)
        -> u64;
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum PaymentError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Paused = 3,
    InvalidAmount = 4,
    InvalidFeeRate = 5,
    MerchantNotAccepting = 6,
    DuplicateOrder = 7,
    InvalidOrderRef = 8,
    PaymentNotFound = 9,
    RefundExceedsPayment = 10,
    AlreadyRefunded = 11,
    Overflow = 12,
    InvalidBatch = 13,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PaymentStatus {
    Completed,
    PartiallyRefunded,
    Refunded,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Payment {
    pub id: u64,
    pub payer: Address,
    pub merchant: Address,
    pub settlement: Address,
    pub asset: Address,
    /// Gross amount the payer authorized.
    pub amount: i128,
    pub fee: i128,
    pub refunded: i128,
    pub order_ref: String,
    pub loyalty_points: u64,
    pub status: PaymentStatus,
    pub created_at: u64,
}

/// One line of a batch payment.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchLine {
    pub merchant: Address,
    pub amount: i128,
    pub order_ref: String,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub admin: Address,
    pub registry: Address,
    pub treasury: Address,
    pub fee_bps: u32,
    pub loyalty: Option<Address>,
    pub paused: bool,
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Config,
    PaymentCount,
    Payment(u64),
    Order(Address, String),
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaymentSettled {
    #[topic]
    pub merchant: Address,
    #[topic]
    pub payment_id: u64,
    pub payer: Address,
    pub asset: Address,
    pub amount: i128,
    pub fee: i128,
    pub order_ref: String,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaymentRefunded {
    #[topic]
    pub merchant: Address,
    #[topic]
    pub payment_id: u64,
    pub amount: i128,
    pub total_refunded: i128,
    pub reason: String,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigUpdated {
    pub fee_bps: u32,
    pub treasury: Address,
    pub paused: bool,
}

// Build metadata embedded in the wasm (SEP-0055), so anyone can trace a
// deployed contract back to its source; see scripts/verify.sh.
soroban_sdk::contractmeta!(key = "source_repo", val = "github:StellarPOS-App/contracts");
soroban_sdk::contractmeta!(key = "home_domain", val = "github.com/StellarPOS-App");
soroban_sdk::contractmeta!(key = "desc", val = "StellarPOS payment processor");

#[contract]
pub struct PaymentProcessor;

#[contractimpl]
impl PaymentProcessor {
    pub fn initialize(
        env: Env,
        admin: Address,
        registry: Address,
        treasury: Address,
        fee_bps: u32,
    ) -> Result<(), PaymentError> {
        if env.storage().instance().has(&DataKey::Config) {
            return Err(PaymentError::AlreadyInitialized);
        }
        admin.require_auth();
        if fee_bps > MAX_FEE_BPS {
            return Err(PaymentError::InvalidFeeRate);
        }
        let config = Config {
            admin,
            registry,
            treasury,
            fee_bps,
            loyalty: None,
            paused: false,
        };
        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().set(&DataKey::PaymentCount, &0u64);
        extend_instance(&env);
        Ok(())
    }

    /// Settles a sale. `order_ref` is the POS order number; it must be unique
    /// per merchant.
    pub fn pay(
        env: Env,
        payer: Address,
        merchant: Address,
        asset: Address,
        amount: i128,
        order_ref: String,
    ) -> Result<u64, PaymentError> {
        let config = Self::config(&env)?;
        if config.paused {
            return Err(PaymentError::Paused);
        }
        payer.require_auth();
        let settlement = Self::settlement_for(&env, &config, &merchant, &asset)?;
        Self::settle(
            &env, &config, &payer, merchant, settlement, &asset, amount, order_ref,
        )
    }

    /// Settles several payments from one payer in a single transaction: a
    /// customer clearing a tab of open orders, or a business paying a batch
    /// of invoices, possibly across merchants. All lines settle or none do;
    /// any invalid line (bad amount, duplicate order, merchant not
    /// accepting) reverts the whole batch. Returns the payment IDs in order.
    pub fn pay_batch(
        env: Env,
        payer: Address,
        asset: Address,
        lines: Vec<BatchLine>,
    ) -> Result<Vec<u64>, PaymentError> {
        let config = Self::config(&env)?;
        if config.paused {
            return Err(PaymentError::Paused);
        }
        if lines.is_empty() || lines.len() > MAX_BATCH {
            return Err(PaymentError::InvalidBatch);
        }
        payer.require_auth();

        // Look each merchant up once, however many lines it has.
        let mut targets: Map<Address, Address> = Map::new(&env);
        let mut ids = Vec::new(&env);
        for line in lines.iter() {
            let settlement = match targets.get(line.merchant.clone()) {
                Some(s) => s,
                None => {
                    let s = Self::settlement_for(&env, &config, &line.merchant, &asset)?;
                    targets.set(line.merchant.clone(), s.clone());
                    s
                }
            };
            ids.push_back(Self::settle(
                &env,
                &config,
                &payer,
                line.merchant,
                settlement,
                &asset,
                line.amount,
                line.order_ref,
            )?);
        }
        Ok(ids)
    }

    /// Refunds part or all of a payment from the merchant's settlement
    /// account back to the payer. The protocol fee is not returned by the
    /// treasury, so the merchant absorbs it on refunds.
    pub fn refund(
        env: Env,
        payment_id: u64,
        amount: i128,
        reason: String,
    ) -> Result<i128, PaymentError> {
        let mut payment = Self::load(&env, payment_id)?;
        payment.settlement.require_auth();

        if amount <= 0 {
            return Err(PaymentError::InvalidAmount);
        }
        if payment.status == PaymentStatus::Refunded {
            return Err(PaymentError::AlreadyRefunded);
        }
        let total = payment
            .refunded
            .checked_add(amount)
            .ok_or(PaymentError::Overflow)?;
        if total > payment.amount {
            return Err(PaymentError::RefundExceedsPayment);
        }

        token::Client::new(&env, &payment.asset).transfer(
            &payment.settlement,
            &payment.payer,
            &amount,
        );

        payment.refunded = total;
        payment.status = if total == payment.amount {
            PaymentStatus::Refunded
        } else {
            PaymentStatus::PartiallyRefunded
        };
        Self::save(&env, &payment);

        PaymentRefunded {
            merchant: payment.merchant,
            payment_id,
            amount,
            total_refunded: total,
            reason,
        }
        .publish(&env);
        Ok(total)
    }

    // ---------------------------------------------------------------------
    // Admin
    // ---------------------------------------------------------------------

    pub fn set_fee_bps(env: Env, fee_bps: u32) -> Result<(), PaymentError> {
        if fee_bps > MAX_FEE_BPS {
            return Err(PaymentError::InvalidFeeRate);
        }
        Self::update(&env, |c| c.fee_bps = fee_bps)
    }

    pub fn set_treasury(env: Env, treasury: Address) -> Result<(), PaymentError> {
        Self::update(&env, |c| c.treasury = treasury)
    }

    /// Enables loyalty accrual through the given loyalty program contract.
    /// Pass `None` to switch accrual off.
    pub fn set_loyalty(env: Env, loyalty: Option<Address>) -> Result<(), PaymentError> {
        Self::update(&env, |c| c.loyalty = loyalty)
    }

    pub fn pause(env: Env) -> Result<(), PaymentError> {
        Self::update(&env, |c| c.paused = true)
    }

    pub fn unpause(env: Env) -> Result<(), PaymentError> {
        Self::update(&env, |c| c.paused = false)
    }

    // ---------------------------------------------------------------------
    // Views
    // ---------------------------------------------------------------------

    pub fn get_payment(env: Env, payment_id: u64) -> Result<Payment, PaymentError> {
        Self::load(&env, payment_id)
    }

    /// Looks up a payment by the POS order reference. The backend uses this
    /// to confirm an order was paid on-chain.
    pub fn payment_for_order(
        env: Env,
        merchant: Address,
        order_ref: String,
    ) -> Result<Payment, PaymentError> {
        let id: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::Order(merchant, order_ref))
            .ok_or(PaymentError::PaymentNotFound)?;
        Self::load(&env, id)
    }

    pub fn payment_count(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::PaymentCount)
            .unwrap_or(0)
    }

    pub fn get_config(env: Env) -> Result<Config, PaymentError> {
        Self::config(&env)
    }

    /// Fee and net split for a prospective payment, for checkout previews.
    pub fn quote(env: Env, amount: i128) -> Result<(i128, i128), PaymentError> {
        if amount <= 0 {
            return Err(PaymentError::InvalidAmount);
        }
        split_fee(amount, Self::config(&env)?.fee_bps).ok_or(PaymentError::Overflow)
    }

    // ---------------------------------------------------------------------
    // Internals
    // ---------------------------------------------------------------------

    /// One registry call: the merchant's settlement account if it is active
    /// and accepts `asset`.
    fn settlement_for(
        env: &Env,
        config: &Config,
        merchant: &Address,
        asset: &Address,
    ) -> Result<Address, PaymentError> {
        RegistryClient::new(env, &config.registry)
            .checkout_target(merchant, asset)
            .ok_or(PaymentError::MerchantNotAccepting)
    }

    /// Validates, transfers and records one payment. The caller has checked
    /// the pause flag and the payer's authorization.
    #[allow(clippy::too_many_arguments)]
    fn settle(
        env: &Env,
        config: &Config,
        payer: &Address,
        merchant: Address,
        settlement: Address,
        asset: &Address,
        amount: i128,
        order_ref: String,
    ) -> Result<u64, PaymentError> {
        if amount <= 0 {
            return Err(PaymentError::InvalidAmount);
        }
        if order_ref.is_empty() || order_ref.len() > MAX_ORDER_REF_LEN {
            return Err(PaymentError::InvalidOrderRef);
        }
        let order_key = DataKey::Order(merchant.clone(), order_ref.clone());
        if env.storage().persistent().has(&order_key) {
            return Err(PaymentError::DuplicateOrder);
        }

        let (fee, net) = split_fee(amount, config.fee_bps).ok_or(PaymentError::Overflow)?;
        let token = token::Client::new(env, asset);
        token.transfer(payer, &settlement, &net);
        if fee > 0 {
            token.transfer(payer, &config.treasury, &fee);
        }

        let loyalty_points = match &config.loyalty {
            Some(loyalty) => LoyaltyClient::new(env, loyalty).accrue(
                &env.current_contract_address(),
                &merchant,
                payer,
                &amount,
            ),
            None => 0,
        };

        let id = Self::payment_count(env.clone()) + 1;
        let payment = Payment {
            id,
            payer: payer.clone(),
            merchant: merchant.clone(),
            settlement,
            asset: asset.clone(),
            amount,
            fee,
            refunded: 0,
            order_ref: order_ref.clone(),
            loyalty_points,
            status: PaymentStatus::Completed,
            created_at: env.ledger().timestamp(),
        };
        Self::save(env, &payment);
        env.storage().persistent().set(&order_key, &id);
        extend_persistent(env, &order_key);
        env.storage().instance().set(&DataKey::PaymentCount, &id);
        extend_instance(env);

        PaymentSettled {
            merchant,
            payment_id: id,
            payer: payer.clone(),
            asset: asset.clone(),
            amount,
            fee,
            order_ref,
        }
        .publish(env);
        Ok(id)
    }

    fn config(env: &Env) -> Result<Config, PaymentError> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(PaymentError::NotInitialized)
    }

    fn update(env: &Env, f: impl FnOnce(&mut Config)) -> Result<(), PaymentError> {
        let mut config = Self::config(env)?;
        config.admin.require_auth();
        f(&mut config);
        env.storage().instance().set(&DataKey::Config, &config);
        extend_instance(env);
        ConfigUpdated {
            fee_bps: config.fee_bps,
            treasury: config.treasury,
            paused: config.paused,
        }
        .publish(env);
        Ok(())
    }

    fn load(env: &Env, id: u64) -> Result<Payment, PaymentError> {
        let key = DataKey::Payment(id);
        let payment = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(PaymentError::PaymentNotFound)?;
        extend_persistent(env, &key);
        Ok(payment)
    }

    fn save(env: &Env, payment: &Payment) {
        let key = DataKey::Payment(payment.id);
        env.storage().persistent().set(&key, payment);
        extend_persistent(env, &key);
    }
}

#[cfg(test)]
mod test;
