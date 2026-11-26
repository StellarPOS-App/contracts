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
//! Marketplace integrations can settle with `pay_with_royalties`, which also
//! pays royalty recipients a share of the net amount (after the platform fee),
//! with the remainder going to the seller's settlement account.
//!
//! Critical parameters (fee rate, treasury) are changed by the admin until
//! `enable_governance` hands them to an on-chain council: from then on they
//! only change through `propose_param_change` / `approve_param_change` once
//! enough council members approve.
//!
//! the funds) back to the original payer, up to the gross amount paid.
//!
//! Every transfer is checked against the recipient's balance, so a token that
//! reports success without moving funds is rejected. Payment IDs are derived
//! from a SHA-256 over (payer, merchant, amount, timestamp, nonce), not a
//! counter, and every sale and refund also emits a `ComplianceRecord` for
//! off-chain reporting tools.
//!
//! # Authorization
//!
//! Every state-changing entry point calls `require_auth` on the account whose
//! funds or configuration it touches: the payer (`pay*`, and each contributor
//! in `split_payment`), the delegatee (`pay_delegated`), the delegator
//! (`create_delegation`, `revoke_delegation`), the merchant
//! (`set_merchant_callback`), the settlement account (`refund`), the admin
//! (config, pause, loyalty, governance) and council members (proposals).

#![no_std]

use shared::{
    bps_of, extend_instance, extend_persistent, is_entry_valid, split_fee, BPS_DENOMINATOR,
    MAX_FEE_BPS,
};
use soroban_sdk::{
    contract, contractclient, contracterror, contractevent, contractimpl, contracttype,
    symbol_short, token, Address, BytesN, Env, IntoVal, Map, String, Symbol, Vec,
};

pub const MAX_ORDER_REF_LEN: u32 = 64;
/// Most payments in one pay_batch call, to stay well inside resource limits.
pub const MAX_BATCH: u32 = 20;
/// Most royalty recipients on one payment.
pub const MAX_ROYALTY_RECIPIENTS: u32 = 5;
/// Largest governance council.
pub const MAX_COUNCIL: u32 = 10;
/// Most contributing buyers in one split_payment call.
pub const MAX_SPLITS: u32 = 20;
pub const DEFAULT_MAX_PAYMENTS_PER_BLOCK: u32 = 20;
pub const MAX_MAX_PAYMENTS_PER_BLOCK: u32 = 100;

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

/// Price-normalization oracle used to calculate fees in a common denomination.
/// The returned fee is converted back into the payment asset before transfer.
#[allow(dead_code)]
#[contractclient(name = "PriceOracleClient")]
pub trait PriceOracleInterface {
    fn normalize(env: Env, asset: Address, amount: i128) -> i128;
    fn denormalize(env: Env, asset: Address, normalized_amount: i128) -> i128;
}

/// Optional merchant callback invoked after a payment settles.
#[allow(dead_code)]
#[contractclient(name = "MerchantCallbackClient")]
pub trait MerchantCallbackInterface {
    fn on_payment_settled(
        env: Env,
        payment_id: u64,
        merchant: Address,
        payer: Address,
        asset: Address,
        amount: i128,
    );
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
    InvalidRoyaltySplit = 14,
    GovernanceActive = 15,
    InvalidGovernance = 16,
    NotCouncilMember = 17,
    ProposalNotFound = 18,
    ProposalExecuted = 19,
    AlreadyApproved = 20,
    /// The token did not credit the recipient with the amount transferred.
    TransferFailed = 21,
    InvalidSplit = 22,
    Reentrancy = 23,
    InvalidRateLimit = 24,
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

/// One royalty recipient and its share of the net amount, in basis points.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoyaltyShare {
    pub recipient: Address,
    pub bps: u32,
}

/// One buyer's contribution in a group split payment.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SplitShare {
    pub payer: Address,
    pub amount: i128,
}

/// The council that controls critical parameters once governance is enabled.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Governance {
    pub council: Vec<Address>,
    /// Approvals needed to apply a change.
    pub threshold: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParamChange {
    FeeBps(u32),
    Treasury(Address),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParamProposal {
    pub id: u64,
    pub change: ParamChange,
    pub approvals: Vec<Address>,
    pub executed: bool,
}
/// One buyer's contribution in a group split payment.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SplitShare {
    pub payer: Address,
    pub amount: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingTreasuryChange {
    pub treasury: Address,
    pub execute_after: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdminRoles {
    pub fee_admin: Address,
    pub dispute_admin: Address,
    pub upgrade_admin: Address,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub admin: Address,
    pub registry: Address,
    pub treasury: Address,
    pub fee_bps: u32,
    pub paused: bool,
    pub max_payments_per_block: u32,
    /// Maximum payment amount allowed (Issue #60: prevent overflow in fee calculations)
    pub max_payment_amount: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MerchantAnalytics {
    pub total_volume: i128,
    pub payment_count: u64,
    /// Basis points; remains zero until dispute records are supported.
    pub dispute_rate: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
struct PaymentRateWindow {
    ledger: u32,
    count: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Delegation {
    pub delegator: Address,
    pub delegatee: Address,
    pub spending_limit: i128,
    pub spent: i128,
    pub expiry: u64,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Config,
    AdminRoles,
    PendingTreasuryChange,
    PriceOracle,
    PaymentCount,
    /// Monotonic nonce mixed into payment IDs; never decremented.
    NextPaymentId,
    MerchantPaymentCount(Address),
    MerchantAnalytics(Address),
    PaymentRate(Address),
    EventIndex,
    Payment(u64),
    Order(Address, String),
    Loyalty,
    Governance,
    ProposalCount,
    Proposal(u64),
    Loyalty,
    Delegation(Address, Address),
    Loyalty,
    MerchantCallback(Address),
    /// KYC verification status for merchants (Issue #57)
    MerchantKyc(Address),
    ReentrancyLock,
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
pub struct PaymentSplit {
    #[topic]
    pub merchant: Address,
    #[topic]
    pub payment_id: u64,
    pub asset: Address,
    pub total_amount: i128,
    pub fee: i128,
    pub contributors: u32,
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
    pub event_index: u64,
}

/// One record per money movement, with every field a compliance report
/// needs. All records share the `compliance` topic, so a reporting tool can
/// follow sales and refunds with a single subscription. `kind` is `payment`
/// (payer to settlement) or `refund` (settlement to payer).
#[contractevent(topics = ["compliance"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComplianceRecord {
    #[topic]
    pub kind: Symbol,
    #[topic]
    pub merchant: Address,
    pub payment_id: u64,
    pub payer: Address,
    pub settlement: Address,
    pub asset: Address,
    pub amount: i128,
    pub fee: i128,
    pub order_ref: String,
    pub timestamp: u64,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoyaltyPaid {
    #[topic]
    pub payment_id: u64,
    #[topic]
    pub recipient: Address,
    pub amount: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParamChangeProposed {
    #[topic]
    pub proposal_id: u64,
    pub proposer: Address,
    pub change: ParamChange,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParamChangeApplied {
    #[topic]
    pub proposal_id: u64,
    pub change: ParamChange,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdminRolesUpdated {
    pub fee_admin: Address,
    pub dispute_admin: Address,
    pub upgrade_admin: Address,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessorUpgraded {
    pub upgrade_admin: Address,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreasuryChangeScheduled {
    pub treasury: Address,
    pub execute_after: u64,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreasuryChangeExecuted {
    pub treasury: Address,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PriceOracleUpdated {
    pub oracle: Option<Address>,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigUpdated {
    pub fee_bps: u32,
    pub treasury: Address,
    pub paused: bool,
    pub max_payment_amount: i128,
    pub event_index: u64,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegationCreated {
    #[topic]
    pub delegator: Address,
    #[topic]
    pub delegatee: Address,
    pub spending_limit: i128,
    pub expiry: u64,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegationRevoked {
    #[topic]
    pub delegator: Address,
    #[topic]
    pub delegatee: Address,
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
    /// Initializes the payment processor.
    ///
    /// # Security
    ///
    /// In production deployments, `admin` **must** be a multisig contract
    /// (such as the deployed `treasury` M-of-N multisig) or a Stellar M-of-N
    /// multisig account rather than a single-key EOA. See `SECURITY.md`.
    pub fn initialize(
        env: Env,
        admin: Address,
        registry: Address,
        treasury: Address,
        fee_bps: u32,
    ) -> Result<(), PaymentError> {
        let storage = env.storage().instance();
        if storage.has(&DataKey::Config) {
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
            paused: false,
            max_payments_per_block: DEFAULT_MAX_PAYMENTS_PER_BLOCK,
            max_payment_amount: i128::MAX, // No limit by default (Issue #60)
        };
        env.storage().instance().set(&DataKey::Config, &config);
        let roles = AdminRoles {
            fee_admin: config.admin.clone(),
            dispute_admin: config.admin.clone(),
            upgrade_admin: config.admin.clone(),
        };
        env.storage().instance().set(&DataKey::AdminRoles, &roles);
        env.storage().instance().set(&DataKey::PaymentCount, &0u64);
        env.storage().instance().set(&DataKey::NextPaymentId, &0u64);
        extend_instance(&env);
        Ok(())
    }

    /// Settles a sale. `order_ref` is the POS order number; it must be unique
    /// per merchant. Returns the payment ID.
    pub fn pay(
        env: Env,
        payer: Address,
        merchant: Address,
        asset: Address,
        amount: i128,
        order_ref: String,
    ) -> Result<u64, PaymentError> {
        let category = String::from_str(&env, "payment");
        Self::pay_with_category(
            env,
            payer,
            merchant,
            asset,
            amount,
            order_ref,
            category,
        )
    }

    pub fn pay_with_category(
        env: Env,
        payer: Address,
        merchant: Address,
        asset: Address,
        amount: i128,
        order_ref: String,
        category: String,
    ) -> Result<u64, PaymentError> {
        let config = Self::config(&env)?;
        if config.paused {
            return Err(PaymentError::Paused);
        }
        payer.require_auth();
        Self::consume_rate_limit(&env, &config, &payer)?;
        let settlement = Self::settlement_for(&env, &config, &merchant, &asset)?;
        Self::settle(
            &env,
            &config,
            &payer,
            merchant,
            settlement,
            &asset,
            amount,
            order_ref,
            category,
            &Vec::new(&env),
        )
    }

    /// Settles a payment on behalf of `delegator`, authorized by the
    /// delegatee (the caller) and capped by the delegator's spending limit.
    /// Fails if the delegation is missing, expired or over its limit.
    ///
    /// The token transfer still needs the delegator's authorization at the
    /// token level, which the delegator grants for this contract.
    pub fn pay_delegated(
        env: Env,
        delegatee: Address,
        delegator: Address,
        delegatee: Address,
        merchant: Address,
        asset: Address,
        amount: i128,
        order_ref: String,
    ) -> Result<u64, PaymentError> {
        let config = Self::config(&env)?;
        if config.paused {
            return Err(PaymentError::Paused);
        }
        if amount <= 0 {
            return Err(PaymentError::InvalidAmount);
        }
        delegator.require_auth();
        Self::consume_rate_limit(&env, &config, &delegator)?;
        let delegatee = env.current_contract_address();
        let mut delegation = Self::get_delegation(&env, &delegator, &delegatee)?;

        if env.ledger().timestamp() > delegation.expiry {
            return Err(PaymentError::InvalidOrderRef);
        }
        if delegation
            .spent
            .checked_add(amount)
            .ok_or(PaymentError::Overflow)?
            > delegation.spending_limit
        {
            return Err(PaymentError::InvalidAmount);
        }

        let settlement = Self::settlement_for(&env, &config, &merchant, &asset)?;
        let payment_id = Self::settle(
            &env,
            &config,
            &delegator,
            merchant,
            settlement,
            &asset,
            amount,
            order_ref,
            String::from_str(&env, ""),
            &Vec::new(&env),
        )?;
        delegation.spent = delegation
            .spent
            .checked_add(amount)
            .ok_or(PaymentError::Overflow)?;
        Self::save_delegation(&env, &delegation);
        Ok(payment_id)
    }

    /// Settles a marketplace sale with optional royalty recipients.
    pub fn pay_with_royalties(
        env: Env, payer: Address, merchant: Address, asset: Address, amount: i128,
        order_ref: String, royalties: Vec<RoyaltyShare>,
    ) -> Result<u64, PaymentError> {
        Self::with_reentrancy_guard(&env, || {
            let config = Self::config(&env)?;
            if config.paused { return Err(PaymentError::Paused); }
            if royalties.len() > MAX_ROYALTY_RECIPIENTS { return Err(PaymentError::InvalidRoyaltySplit); }
            let mut total_bps: u32 = 0;
            for share in royalties.iter() {
                if share.bps == 0 { return Err(PaymentError::InvalidRoyaltySplit); }
                total_bps = total_bps.checked_add(share.bps).ok_or(PaymentError::Overflow)?;
            }
            total_bps = total_bps.saturating_add(share.bps);
        }
        if total_bps > BPS_DENOMINATOR {
            return Err(PaymentError::InvalidRoyaltySplit);
        }
        payer.require_auth();
        Self::consume_rate_limit(&env, &config, &payer)?;
        let settlement = Self::settlement_for(&env, &config, &merchant, &asset)?;
        Self::settle(
            &env,
            &config,
            &payer,
            merchant,
            settlement,
            &asset,
            amount,
            order_ref,
            String::from_str(&env, ""),
            &royalties,
        )?;
        Ok(payment_id)
    }

    /// Settles several payments atomically.
    pub fn pay_batch(
        env: Env, payer: Address, asset: Address, lines: Vec<BatchLine>,
    ) -> Result<Vec<u64>, PaymentError> {
        Self::with_reentrancy_guard(&env, || {
            let config = Self::active_config(&env)?;
            if lines.is_empty() || lines.len() > MAX_BATCH {
                return Err(PaymentError::InvalidBatch);
            }
            payer.require_auth_for_args((asset.clone(), lines.clone()).into_val(&env));

            // Look each merchant up once, however many lines it has.
            let mut targets: Map<Address, Address> = Map::new(&env);
            let mut ids = Vec::new(&env);
            for line in lines.iter() {
                if line.amount <= 0 {
                    return Err(PaymentError::InvalidAmount);
                }
                Self::consume_rate_limit(&env, &config, &payer)?;
                let settlement = match targets.get(line.merchant.clone()) {
                    Some(value) => value,
                    None => {
                        let value = Self::settlement_for(&env, &config, &line.merchant, &asset)?;
                        targets.set(line.merchant.clone(), value.clone());
                        value
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
                    line.category,
                    &Vec::new(&env),
                )?);
            }
            Ok(ids)
        })
    }

    /// Refunds part or all of a payment from the merchant settlement account.
    pub fn refund(
        env: Env, payment_id: u64, amount: i128, reason: String,
    ) -> Result<i128, PaymentError> {
        Self::with_reentrancy_guard(&env, || {
            let config = Self::config(&env)?;
            if config.paused {
                return Err(PaymentError::Paused);
            }
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

            let client = token::Client::new(&env, &payment.asset);
            Self::transfer_checked(&client, &payment.settlement, &payment.payer, amount)?;

            payment.refunded = total;
            payment.status = if total == payment.amount { PaymentStatus::Refunded } else { PaymentStatus::PartiallyRefunded };
            Self::save(&env, &payment);
            if payment.status == PaymentStatus::Refunded {
                let active = Self::payment_count(env.clone()).saturating_sub(1);
                env.storage()
                    .instance()
                    .set(&DataKey::PaymentCount, &active);
                let m_key = DataKey::MerchantPaymentCount(payment.merchant.clone());
                let m_active = Self::merchant_payment_count(env.clone(), payment.merchant.clone())
                    .saturating_sub(1);
                env.storage().persistent().set(&m_key, &m_active);
                extend_persistent(&env, &m_key);
                extend_instance(&env);
            }
            ComplianceRecord {
                kind: symbol_short!("refund"),
                merchant: payment.merchant.clone(),
                payment_id,
                payer: payment.payer.clone(),
                settlement: payment.settlement.clone(),
                asset: payment.asset.clone(),
                amount,
                fee: 0,
                order_ref: payment.order_ref.clone(),
                timestamp: env.ledger().timestamp(),
            }
            .publish(&env);
            PaymentRefunded {
                merchant: payment.merchant,
                payment_id,
                amount,
                total_refunded: total,
                reason,
                event_index: Self::next_event_index(&env),
            }
            ComplianceRecord {
                kind: symbol_short!("refund"), merchant: payment.merchant.clone(), payment_id,
                payer: payment.payer.clone(), settlement: payment.settlement.clone(), asset: payment.asset.clone(),
                amount, fee: 0, order_ref: payment.order_ref.clone(), timestamp: env.ledger().timestamp(),
            }.publish(&env);
            PaymentRefunded {
                merchant: payment.merchant, payment_id, amount, total_refunded: total, reason,
                event_index: Self::next_event_index(&env),
            }.publish(&env);
            Ok(total)
        })
    }

    pub fn get_merchant_callback(env: Env, merchant: Address) -> Option<Address> {
        let key = DataKey::MerchantCallback(merchant);
        let addr = env.storage().persistent().get(&key);
        if addr.is_some() {
            extend_persistent(&env, &key);
        }
        addr
    }

    // ---------------------------------------------------------------------
    // Admin
    // ---------------------------------------------------------------------

    /// Admin-only until governance is enabled; then use `propose_param_change`.
    pub fn set_fee_bps(env: Env, fee_bps: u32) -> Result<(), PaymentError> {
        Self::require_no_governance(&env)?;
        if fee_bps > MAX_FEE_BPS {
            return Err(PaymentError::InvalidFeeRate);
        }
        let roles = Self::admin_roles(&env)?;
        roles.fee_admin.require_auth();
        let config = Self::config(&env)?;
        Self::store_config(&env, config, |c| c.fee_bps = fee_bps)
    }

    /// Admin-only until governance is enabled; then use `propose_param_change`.
    pub fn set_treasury(env: Env, treasury: Address) -> Result<(), PaymentError> {
        Self::require_no_governance(&env)?;
        let roles = Self::admin_roles(&env)?;
        roles.fee_admin.require_auth();
        Self::schedule_treasury_change(&env, treasury)
    }

    pub fn get_admin_roles(env: Env) -> Result<AdminRoles, PaymentError> {
        Self::admin_roles(&env)
    }

    pub fn set_admin_roles(
        env: Env,
        fee_admin: Address,
        dispute_admin: Address,
        upgrade_admin: Address,
    ) -> Result<(), PaymentError> {
        let config = Self::config(&env)?;
        config.admin.require_auth();
        let roles = AdminRoles {
            fee_admin: fee_admin.clone(),
            dispute_admin: dispute_admin.clone(),
            upgrade_admin: upgrade_admin.clone(),
        };
        env.storage().instance().set(&DataKey::AdminRoles, &roles);
        extend_instance(&env);
        AdminRolesUpdated { fee_admin, dispute_admin, upgrade_admin }.publish(&env);
        Ok(())
    }

    /// Sets the maximum payment amount. Admin-only.
    /// Issue #60: Add maximum payment amount validation
    pub fn set_max_payment_amount(env: Env, max_amount: i128) -> Result<(), PaymentError> {
        Self::require_no_governance(&env)?;
        if max_amount <= 0 {
            return Err(PaymentError::InvalidAmount);
        }
        Self::update(&env, |c| c.max_payment_amount = max_amount)
    }

    /// Sets the KYC verification status for a merchant. Admin-only.
    /// Issue #57: Add merchant KYC flag on-chain
    pub fn set_merchant_kyc(env: Env, merchant: Address, verified: bool) -> Result<(), PaymentError> {
        let config = Self::config(&env)?;
        config.admin.require_auth();
        let key = DataKey::MerchantKyc(merchant);
        if verified {
            env.storage().persistent().set(&key, &true);
            extend_persistent(&env, &key);
        } else {
            env.storage().persistent().remove(&key);
        }
        Ok(())
    }

    /// Gets the KYC verification status of a merchant.
    /// Issue #57: Add merchant KYC flag on-chain
    pub fn get_merchant_kyc(env: Env, merchant: Address) -> bool {
        let key = DataKey::MerchantKyc(merchant);
        env.storage().persistent().get(&key).unwrap_or(false)
    }

    pub fn upgrade(env: Env, wasm_hash: BytesN<32>) -> Result<(), PaymentError> {
        let roles = Self::admin_roles(&env)?;
        roles.upgrade_admin.require_auth();
        let actor = roles.upgrade_admin.clone();
        env.deployer().update_current_contract_wasm(wasm_hash);
        ProcessorUpgraded { upgrade_admin: actor }.publish(&env);
        Ok(())
    }

    /// Executes a previously approved treasury redirect after the seven-day
    /// safety delay. Permissionless execution prevents an admin from holding
    /// an approved change hostage while preserving the full timelock.
    pub fn execute_treasury_change(env: Env) -> Result<(), PaymentError> {
        let pending: PendingTreasuryChange = env
            .storage()
            .instance()
            .get(&DataKey::PendingTreasuryChange)
            .ok_or(PaymentError::NoPendingTreasuryChange)?;
        if env.ledger().timestamp() < pending.execute_after {
            return Err(PaymentError::TreasuryChangeNotReady);
        }
        let config = Self::config(&env)?;
        let treasury = pending.treasury.clone();
        Self::store_config(&env, config, |c| c.treasury = treasury.clone())?;
        env.storage().instance().remove(&DataKey::PendingTreasuryChange);
        extend_instance(&env);
        TreasuryChangeExecuted { treasury }.publish(&env);
        Ok(())
    }

    pub fn get_pending_treasury_change(env: Env) -> Option<PendingTreasuryChange> {
        env.storage().instance().get(&DataKey::PendingTreasuryChange)
    }

    /// Configures the oracle that normalizes payment values before fee math.
    /// `None` keeps the legacy same-denomination calculation.
    pub fn set_price_oracle(env: Env, oracle: Option<Address>) -> Result<(), PaymentError> {
        let config = Self::config(&env)?;
        config.admin.require_auth();
        match oracle.clone() {
            Some(address) => env.storage().instance().set(&DataKey::PriceOracle, &address),
            None => env.storage().instance().remove(&DataKey::PriceOracle),
        }
        extend_instance(&env);
        PriceOracleUpdated { oracle }.publish(&env);
        Ok(())
    }

    pub fn get_price_oracle(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::PriceOracle)
    }

    /// Sets the maximum number of payments one payer may create per ledger.
    pub fn set_max_payments_per_block(env: Env, limit: u32) -> Result<(), PaymentError> {
        if limit == 0 || limit > MAX_MAX_PAYMENTS_PER_BLOCK {
            return Err(PaymentError::InvalidRateLimit);
        }
        Self::update(&env, |c| c.max_payments_per_block = limit)
    }

    // ---------------------------------------------------------------------
    // Governance
    // ---------------------------------------------------------------------

    /// Hands control of the fee rate and treasury to a council. One-way: the
    /// admin can no longer change those parameters on its own afterwards.
    pub fn enable_governance(
        env: Env,
        council: Vec<Address>,
        threshold: u32,
    ) -> Result<(), PaymentError> {
        let config = Self::config(&env)?;
        config.admin.require_auth();
        Self::require_no_governance(&env)?;
        if council.is_empty()
            || council.len() > MAX_COUNCIL
            || threshold == 0
            || threshold > council.len()
        {
            return Err(PaymentError::InvalidGovernance);
        }
        for (i, member) in council.iter().enumerate() {
            if council.first_index_of(&member) != Some(i as u32) {
                return Err(PaymentError::InvalidGovernance);
            }
        }
        let gov = Governance { council, threshold };
        env.storage().instance().set(&DataKey::Governance, &gov);
        extend_instance(&env);
        Ok(())
    }

    /// A council member proposes a parameter change; the proposer's approval
    /// is counted, so a threshold of 1 applies it immediately.
    pub fn propose_param_change(
        env: Env,
        proposer: Address,
        change: ParamChange,
    ) -> Result<u64, PaymentError> {
        let gov = Self::governance(&env)?;
        Self::require_member(&gov, &proposer)?;
        if let ParamChange::FeeBps(bps) = change {
            if bps > MAX_FEE_BPS {
                return Err(PaymentError::InvalidFeeRate);
            }
        }
        let id: u64 = env
            .storage()
            .instance()
            .get::<_, u64>(&DataKey::ProposalCount)
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(PaymentError::Overflow)?;
        env.storage().instance().set(&DataKey::ProposalCount, &id);
        let mut proposal = ParamProposal {
            id,
            change: change.clone(),
            approvals: Vec::from_array(&env, [proposer.clone()]),
            executed: false,
        };
        ParamChangeProposed {
            proposal_id: id,
            proposer,
            change,
        }
        .publish(&env);
        Self::apply_if_ready(&env, &gov, &mut proposal)?;
        Ok(id)
    }

    /// A council member approves a pending change. Returns `true` once the
    /// change has been applied.
    pub fn approve_param_change(
        env: Env,
        member: Address,
        proposal_id: u64,
    ) -> Result<bool, PaymentError> {
        let gov = Self::governance(&env)?;
        Self::require_member(&gov, &member)?;
        let mut proposal = Self::get_param_proposal(env.clone(), proposal_id)?;
        if proposal.executed {
            return Err(PaymentError::ProposalExecuted);
        }
        if proposal.approvals.contains(&member) {
            return Err(PaymentError::AlreadyApproved);
        }
        proposal.approvals.push_back(member);
        Self::apply_if_ready(&env, &gov, &mut proposal)
    }

    pub fn get_governance(env: Env) -> Option<Governance> {
        env.storage().instance().get(&DataKey::Governance)
    }

    pub fn get_param_proposal(env: Env, proposal_id: u64) -> Result<ParamProposal, PaymentError> {
        let key = DataKey::Proposal(proposal_id);
        let proposal = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(PaymentError::ProposalNotFound)?;
        extend_persistent(&env, &key);
        Ok(proposal)
    }

    /// Enables loyalty accrual through the given loyalty program contract.
    /// Pass `None` to switch accrual off.
    pub fn set_loyalty(env: Env, loyalty: Option<Address>) -> Result<(), PaymentError> {
        let config = Self::config(&env)?;
        config.admin.require_auth();
        match loyalty {
            Some(addr) => env.storage().instance().set(&DataKey::Loyalty, &addr),
            None => env.storage().instance().remove(&DataKey::Loyalty),
        }
        extend_instance(&env);
        Ok(())
    }

    pub fn pause(env: Env) -> Result<(), PaymentError> {
        let roles = Self::admin_roles(&env)?;
        roles.dispute_admin.require_auth();
        let config = Self::config(&env)?;
        Self::store_config(&env, config, |c| c.paused = true)
    }

    pub fn unpause(env: Env) -> Result<(), PaymentError> {
        let roles = Self::admin_roles(&env)?;
        roles.dispute_admin.require_auth();
        let config = Self::config(&env)?;
        Self::store_config(&env, config, |c| c.paused = false)
    }

    // ---------------------------------------------------------------------
    // Delegation Management
    // ---------------------------------------------------------------------

    /// Authorizes a delegatee to make payments on behalf of the delegator,
    /// up to a specified spending limit that expires at a given timestamp.
    pub fn create_delegation(
        env: Env,
        delegator: Address,
        delegatee: Address,
        spending_limit: i128,
        expiry: u64,
    ) -> Result<(), PaymentError> {
        delegator.require_auth();
        if spending_limit <= 0 {
            return Err(PaymentError::InvalidAmount);
        }
        if expiry <= env.ledger().timestamp() {
            return Err(PaymentError::DelegationExpired);
        }

        let delegation = Delegation {
            delegator: delegator.clone(),
            delegatee: delegatee.clone(),
            spending_limit,
            spent: 0,
            expiry,
        };
        Self::save_delegation(&env, &delegation);

        DelegationCreated {
            delegator,
            delegatee,
            spending_limit,
            expiry,
        }
        .publish(&env);
        Ok(())
    }

    /// Revokes a delegation, preventing the delegatee from making further
    /// payments on behalf of the delegator.
    pub fn revoke_delegation(
        env: Env,
        delegator: Address,
        delegatee: Address,
    ) -> Result<(), PaymentError> {
        delegator.require_auth();
        let key = DataKey::Delegation(delegator.clone(), delegatee.clone());
        env.storage().persistent().remove(&key);

        DelegationRevoked {
            delegator,
            delegatee,
        }
        .publish(&env);
        Ok(())
    }

    /// Gets the current delegation between delegator and delegatee.
    pub fn get_delegation_info(
        env: Env,
        delegator: Address,
        delegatee: Address,
    ) -> Result<Delegation, PaymentError> {
        Self::get_delegation(&env, &delegator, &delegatee)
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
        let storage = env.storage().persistent();
        let id: u64 = storage
            .get(&DataKey::Order(merchant, order_ref))
            .ok_or(PaymentError::PaymentNotFound)?;
        Self::load(&env, id)
    }

    /// Number of active (non-fully-refunded) payments.
    pub fn payment_count(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::PaymentCount)
            .unwrap_or(0)
    }

    /// Number of active (non-fully-refunded) payments for a specific merchant.
    pub fn merchant_payment_count(env: Env, merchant: Address) -> u64 {
        env.storage()
            .persistent()
            .get(&DataKey::MerchantPaymentCount(merchant))
            .unwrap_or(0)
    }

    /// Returns aggregate payment data for a merchant. `dispute_rate` is zero
    /// until dispute records are part of this processor's state model.
    pub fn merchant_analytics(env: Env, merchant: Address) -> MerchantAnalytics {
        env.storage()
            .persistent()
            .get(&DataKey::MerchantAnalytics(merchant))
            .unwrap_or(MerchantAnalytics {
                total_volume: 0,
                payment_count: 0,
                dispute_rate: 0,
            })
    }

    pub fn merchant_total_volume(env: Env, merchant: Address) -> i128 {
        Self::merchant_analytics(env, merchant).total_volume
    }

    pub fn merchant_dispute_rate(env: Env, merchant: Address) -> u32 {
        Self::merchant_analytics(env, merchant).dispute_rate
    }

    pub fn get_config(env: Env) -> Result<Config, PaymentError> {
        Self::config(&env)
    }

    pub fn get_loyalty(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Loyalty)
    }

    /// Fee and net split for a prospective payment, for checkout previews.
    pub fn quote(env: Env, amount: i128) -> Result<(i128, i128), PaymentError> {
        if amount <= 0 {
            return Err(PaymentError::InvalidAmount);
        }
        split_fee(amount, Self::config(&env)?.fee_bps).ok_or(PaymentError::Overflow)
    }

    /// Asset-aware fee quote. When an oracle is configured the amount is
    /// normalized into the common denomination before fee calculation (#107).
    pub fn quote_in_asset(
        env: Env,
        asset: Address,
        amount: i128,
    ) -> Result<(i128, i128), PaymentError> {
        if amount <= 0 {
            return Err(PaymentError::InvalidAmount);
        }
        let config = Self::config(&env)?;
        Self::fee_split_for_asset(&env, &config, &asset, amount)
    }

    // ---------------------------------------------------------------------
    // Internals
    // ---------------------------------------------------------------------

    /// The config, or `Paused` while the contract is paused.
    fn active_config(env: &Env) -> Result<Config, PaymentError> {
        let config = Self::config(env)?;
        if config.paused {
            return Err(PaymentError::Paused);
        }
        Ok(config)
    }

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

    fn consume_rate_limit(env: &Env, config: &Config, payer: &Address) -> Result<(), PaymentError> {
        let key = DataKey::PaymentRate(payer.clone());
        let ledger = env.ledger().sequence();
        let mut window = env
            .storage()
            .persistent()
            .get::<_, PaymentRateWindow>(&key)
            .unwrap_or(PaymentRateWindow { ledger, count: 0 });
        if window.ledger != ledger {
            window = PaymentRateWindow { ledger, count: 0 };
        }
        if window.count >= config.max_payments_per_block {
            return Err(PaymentError::InvalidRateLimit);
        }
        window.count = window.count.checked_add(1).ok_or(PaymentError::Overflow)?;
        env.storage().persistent().set(&key, &window);
        extend_persistent(env, &key);
        Ok(())
    }

    /// Validates, transfers and records one payment. The caller has checked
    /// the pause flag and the payer's authorization. When `expected_fee` is
    /// given it must equal the fee computed from the current fee rate.
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
        category: String,
        royalties: &Vec<RoyaltyShare>,
    ) -> Result<u64, PaymentError> {
        if amount <= 0 { return Err(PaymentError::InvalidAmount); }
        // Issue #60: Enforce maximum payment amount to prevent overflow in fee calculations
        if amount > config.max_payment_amount {
            return Err(PaymentError::InvalidAmount);
        }
        if order_ref.is_empty() || order_ref.len() > MAX_ORDER_REF_LEN { return Err(PaymentError::InvalidOrderRef); }
        let order_key = DataKey::Order(merchant.clone(), order_ref.clone());
        if is_entry_valid(env, &order_key) {
            return Err(PaymentError::DuplicateOrder);
        }

        let (fee, net) = split_fee(amount, config.fee_bps).ok_or(PaymentError::Overflow)?;
        let count = Self::payment_count(env.clone())
            .checked_add(1)
            .ok_or(PaymentError::Overflow)?;
        let id = Self::new_payment_id(env, payer, &merchant, &order_ref, count);
        let token = token::Client::new(env, asset);
        let mut seller_net = net;
        let mut royalty_paid: Vec<(Address, i128)> = Vec::new(env);
        for share in royalties.iter() {
            let cut = bps_of(net, share.bps).ok_or(PaymentError::Overflow)?;
            seller_net = seller_net.checked_sub(cut).ok_or(PaymentError::Overflow)?;
            if cut > 0 {
                Self::transfer_checked(&token, payer, &share.recipient, cut)?;
                seller_net -= cut;
                royalty_paid.push_back((share.recipient, cut));
            }
        }
        let client = token::Client::new(env, asset);
        if seller_net > 0 {
            Self::transfer_checked(&client, payer, &settlement, seller_net)?;
        }
        if fee > 0 {
            Self::transfer_checked(&client, payer, &config.treasury, fee)?;
        }

        let loyalty_points = match env
            .storage()
            .instance()
            .get::<_, Address>(&DataKey::Loyalty)
        {
            Some(loyalty) => LoyaltyClient::new(env, &loyalty).accrue(
                &env.current_contract_address(), &merchant, payer, &amount,
            ),
            None => 0,
        }
    }

        let m_count = Self::merchant_payment_count(env.clone(), merchant.clone())
            .checked_add(1)
            .ok_or(PaymentError::Overflow)?;
        let payment = Payment {
            id, payer: payer.clone(), merchant: merchant.clone(), settlement, asset: asset.clone(), amount, fee,
            refunded: 0, order_ref: order_ref.clone(), category: category.clone(), loyalty_points,
            status: PaymentStatus::Completed, created_at: env.ledger().timestamp(), chain_id: env.ledger().network_id(),
        };
        Self::save(env, &payment);
        env.storage().persistent().set(&order_key, &id);
        extend_persistent(env, &order_key);
        let m_key = DataKey::MerchantPaymentCount(merchant.clone());
        env.storage().persistent().set(&m_key, &merchant_count);
        extend_persistent(env, &m_key);
        let analytics_key = DataKey::MerchantAnalytics(merchant.clone());
        let mut analytics = env
            .storage()
            .persistent()
            .get::<_, MerchantAnalytics>(&analytics_key)
            .unwrap_or(MerchantAnalytics {
                total_volume: 0,
                payment_count: 0,
                dispute_rate: 0,
            });
        analytics.total_volume = analytics
            .total_volume
            .checked_add(amount)
            .ok_or(PaymentError::Overflow)?;
        analytics.payment_count = analytics
            .payment_count
            .checked_add(1)
            .ok_or(PaymentError::Overflow)?;
        env.storage().persistent().set(&analytics_key, &analytics);
        extend_persistent(env, &analytics_key);
        env.storage().instance().set(&DataKey::NextPaymentId, &id);
        env.storage().instance().set(&DataKey::PaymentCount, &count);
        extend_instance(env);
        ComplianceRecord {
            kind: symbol_short!("payment"), merchant: merchant.clone(), payment_id: id, payer: payer.clone(),
            settlement: payment.settlement.clone(), asset: asset.clone(), amount, fee, order_ref: order_ref.clone(),
            timestamp: payment.created_at,
        }.publish(env);
        PaymentSettled {
            merchant: merchant.clone(), payment_id: id, payer: payer.clone(), asset: asset.clone(), amount, fee,
            order_ref, category, chain_id: env.ledger().network_id(),
        }.publish(env);
        Self::invoke_merchant_callback(env, id, &merchant, payer, asset, amount);
        Ok(id)
    }

    /// Transfers `amount` and confirms the recipient was credited exactly
    /// that, so a token that reports success without moving funds (or skims
    /// a fee) is rejected and the whole call reverts.
    fn transfer_checked(
        client: &token::Client,
        from: &Address,
        to: &Address,
        amount: i128,
    ) -> Result<(), PaymentError> {
        if from == to {
            client.transfer(from, to, &amount);
            return Ok(());
        }
        let expected = client
            .balance(to)
            .checked_add(amount)
            .ok_or(PaymentError::Overflow)?;
        client.transfer(from, to, &amount);
        if client.balance(to) != expected {
            return Err(PaymentError::TransferFailed);
        }
        Ok(())
    }

    /// The first 8 bytes of a SHA-256 over (payer, merchant, amount,
    /// timestamp, nonce), where the nonce is a counter that only moves
    /// forward. IDs are therefore unpredictable from the counter, and two
    /// payments in the same ledger can never collide because their nonces
    /// differ. A hash collision with an existing ID (or zero) bumps a local
    /// salt and tries again.
    fn new_payment_id(
        env: &Env, payer: &Address, merchant: &Address, order_ref: &String, sequence: u64,
    ) -> u64 {
        let timestamp = env.ledger().timestamp();
        let mut salt: u32 = 0;
        loop {
            let seed = (merchant.clone(), order_ref.clone(), payer.clone(), sequence, env.ledger().sequence(), nonce);
            let digest = env.crypto().sha256(&seed.to_xdr(env)).to_array();
            let mut head = [0u8; 8];
            head.copy_from_slice(&digest[..8]);
            let id = u64::from_be_bytes(head);
            if id != 0 && !env.storage().persistent().has(&DataKey::Payment(id)) {
                return id;
            }
            nonce += 1;
        }
    }
    fn with_reentrancy_guard<T>(
        env: &Env,
        f: impl FnOnce() -> Result<T, PaymentError>,
    ) -> Result<T, PaymentError> {
        if env
            .storage()
            .instance()
            .get(&DataKey::ReentrancyLock)
            .unwrap_or(false)
        {
            return Err(PaymentError::Reentrancy);
        }
        env.storage()
            .instance()
            .set(&DataKey::ReentrancyLock, &true);
        let result = f();
        env.storage()
            .instance()
            .set(&DataKey::ReentrancyLock, &false);
        result
    }

    fn invoke_merchant_callback(
        env: &Env,
        payment_id: u64,
        merchant: &Address,
        payer: &Address,
        asset: &Address,
        amount: i128,
    ) {
        let key = DataKey::MerchantCallback(merchant.clone());
        if let Some(callback) = env.storage().persistent().get::<DataKey, Address>(&key) {
            extend_persistent(env, &key);
            MerchantCallbackClient::new(env, &callback).on_payment_settled(
                &payment_id,
                merchant,
                payer,
                asset,
                &amount,
            );
        }
    }

    fn schedule_treasury_change(env: &Env, treasury: Address) -> Result<(), PaymentError> {
        let execute_after = env
            .ledger()
            .timestamp()
            .checked_add(TREASURY_CHANGE_DELAY)
            .ok_or(PaymentError::Overflow)?;
        let pending = PendingTreasuryChange {
            treasury: treasury.clone(),
            execute_after,
        };
        env.storage()
            .instance()
            .set(&DataKey::PendingTreasuryChange, &pending);
        extend_instance(env);
        TreasuryChangeScheduled { treasury, execute_after }.publish(env);
        Ok(())
    }

    fn fee_split_for_asset(
        env: &Env,
        config: &Config,
        asset: &Address,
        amount: i128,
    ) -> Result<(i128, i128), PaymentError> {
        let Some(oracle) = env.storage().instance().get::<_, Address>(&DataKey::PriceOracle) else {
            return split_fee(amount, config.fee_bps).ok_or(PaymentError::Overflow);
        };
        let oracle = PriceOracleClient::new(env, &oracle);
        let normalized = oracle.normalize(asset, &amount);
        if normalized <= 0 {
            return Err(PaymentError::InvalidOracleQuote);
        }
        let normalized_fee =
            bps_of(normalized, config.fee_bps).ok_or(PaymentError::Overflow)?;
        let fee = oracle.denormalize(asset, &normalized_fee);
        if fee < 0 || fee > amount {
            return Err(PaymentError::InvalidOracleQuote);
        }
        let net = amount.checked_sub(fee).ok_or(PaymentError::Overflow)?;
        Ok((fee, net))
    }

    fn config(env: &Env) -> Result<Config, PaymentError> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(PaymentError::NotInitialized)
    }

    fn admin_roles(env: &Env) -> Result<AdminRoles, PaymentError> {
        if let Some(roles) = env.storage().instance().get::<_, AdminRoles>(&DataKey::AdminRoles) {
            return Ok(roles);
        }
        let config = Self::config(env)?;
        Ok(AdminRoles {
            fee_admin: config.admin.clone(),
            dispute_admin: config.admin.clone(),
            upgrade_admin: config.admin,
        })
    }

    fn update(env: &Env, f: impl FnOnce(&mut Config)) -> Result<(), PaymentError> {
        let config = Self::config(env)?;
        config.admin.require_auth();
        Self::store_config(env, config, f)
    }

    /// Applies `f` to the config and publishes the change. Callers check auth.
    fn store_config(
        env: &Env,
        mut config: Config,
        f: impl FnOnce(&mut Config),
    ) -> Result<(), PaymentError> {
        f(&mut config);
        env.storage().instance().set(&DataKey::Config, &config);
        extend_instance(env);
        ConfigUpdated {
            fee_bps: config.fee_bps,
            treasury: config.treasury,
            paused: config.paused,
            max_payment_amount: config.max_payment_amount,
            event_index: Self::next_event_index(env),
        }
        .publish(env);
        Ok(())
    }

    fn governance(env: &Env) -> Result<Governance, PaymentError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(PaymentError::InvalidGovernance)
    }

    fn require_no_governance(env: &Env) -> Result<(), PaymentError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(PaymentError::GovernanceActive);
        }
        Ok(())
    }

    fn require_member(gov: &Governance, member: &Address) -> Result<(), PaymentError> {
        if !gov.council.contains(member) {
            return Err(PaymentError::NotCouncilMember);
        }
        member.require_auth();
        Ok(())
    }

    /// Saves the proposal, applying its change first when it has enough approvals.
    fn apply_if_ready(
        env: &Env, gov: &Governance, proposal: &mut ParamProposal,
    ) -> Result<bool, PaymentError> {
        if proposal.approvals.len() >= gov.threshold {
            let config = Self::config(env)?;
            match proposal.change.clone() {
                ParamChange::FeeBps(bps) => {
                    if bps > MAX_FEE_BPS {
                        return Err(PaymentError::InvalidFeeRate);
                    }
                    Self::store_config(env, config, |c| c.fee_bps = bps)?
                }
                ParamChange::Treasury(t) => Self::store_config(env, config, |c| c.treasury = t)?,
            }
            proposal.executed = true;
            ParamChangeApplied { proposal_id: proposal.id, change: proposal.change.clone() }.publish(env);
        }
        let key = DataKey::Proposal(proposal.id);
        env.storage().persistent().set(&key, proposal);
        extend_persistent(env, &key);
        Ok(proposal.executed)
    }

    /// Global monotonic counter in persistent storage, so event indices are
    /// never reissued across upgrades or payment restarts.
    fn next_event_index(env: &Env) -> u64 {
        let idx: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::EventIndex)
            .unwrap_or(0)
            + 1;
        env.storage().persistent().set(&DataKey::EventIndex, &idx);
        extend_persistent(env, &DataKey::EventIndex);
        idx
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

    fn get_delegation(
        env: &Env,
        delegator: &Address,
        delegatee: &Address,
    ) -> Result<Delegation, PaymentError> {
        let key = DataKey::Delegation(delegator.clone(), delegatee.clone());
        if !env.storage().persistent().has(&key) {
            return Err(PaymentError::DelegationNotFound);
        }
        let delegation = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(PaymentError::DelegationNotFound)?;
        extend_persistent(env, &key);
        Ok(delegation)
    }

    fn save_delegation(env: &Env, delegation: &Delegation) {
        let key = DataKey::Delegation(delegation.delegator.clone(), delegation.delegatee.clone());
        env.storage().persistent().set(&key, delegation);
        extend_persistent(env, &key);
    }
}

#[cfg(test)]
mod test;
