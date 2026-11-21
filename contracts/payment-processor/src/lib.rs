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
//! Refunds are pushed by the merchant's settlement account (which received
//! the funds) back to the original payer, up to the gross amount paid.
//!
//! Every transfer is checked against the recipient's balance, so a token that
//! reports success without moving funds is rejected. Payment IDs are derived
//! from a hash, not a counter, and every sale and refund also emits a
//! `ComplianceRecord` for off-chain reporting tools.

#![no_std]

use shared::{bps_of, extend_instance, extend_persistent, split_fee, BPS_DENOMINATOR, MAX_FEE_BPS};
use shared::{extend_instance, extend_persistent, is_entry_valid, split_fee, MAX_FEE_BPS};
use soroban_sdk::{
    contract, contractclient, contracterror, contractevent, contractimpl, contracttype, token,
    Address, BytesN, Env, Map, String, Vec,
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
 stellar-fixes-consolidated
    ExpiredLedgerEntry = 14,

    /// The token did not credit the recipient with the amount transferred.
    TransferFailed = 14,
    InvalidSplit = 14,
    Reentrancy = 15,
 main
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
    pub category: String,
    pub loyalty_points: u64,
    pub status: PaymentStatus,
    pub created_at: u64,
    pub chain_id: BytesN<32>,
}

/// One line of a batch payment.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchLine {
    pub merchant: Address,
    pub amount: i128,
    pub order_ref: String,
    pub discount: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub admin: Address,
    pub registry: Address,
    pub treasury: Address,
    pub fee_bps: u32,
    pub paused: bool,
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
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RecurringStatus {
    Active,
    Paused,
    Completed,
    Cancelled,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecurringPayment {
    pub id: u64,
    pub payer: Address,
    pub merchant: Address,
    pub asset: Address,
    pub amount: i128,
    pub interval: u64,
    pub max_occurrences: u64,
    pub occurrences: u64,
    pub next_payment_time: u64,
    pub status: RecurringStatus,
    pub created_at: u64,
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Config,
    PaymentCount,
    NextPaymentId,
    MerchantPaymentCount(Address),
    EventIndex,
    Payment(u64),
    Order(Address, String),
    Governance,
    ProposalCount,
    Proposal(u64),
 stellar-fixes-consolidated
    Delegation(Address, Address),
    RecurringCount,
    RecurringPayment(u64),
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
    pub category: String,
    pub chain_id: BytesN<32>,
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
pub struct ConfigUpdated {
    pub fee_bps: u32,
    pub treasury: Address,
    pub paused: bool,
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

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecurringPaymentCreated {
    #[topic]
    pub recurring_id: u64,
    #[topic]
    pub payer: Address,
    pub merchant: Address,
    pub amount: i128,
    pub interval: u64,
    pub max_occurrences: u64,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecurringPaymentExecuted {
    #[topic]
    pub recurring_id: u64,
    pub payment_id: u64,
    pub occurrence: u64,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecurringPaymentCancelled {
    #[topic]
    pub recurring_id: u64,
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
        };
        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().set(&DataKey::PaymentCount, &0u64);
        env.storage().instance().set(&DataKey::NextPaymentId, &0u64);
        storage.set(&DataKey::Config, &config);
        storage.set(&DataKey::PaymentCount, &0u64);
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
        category: String,
    ) -> Result<u64, PaymentError> {
        Self::pay_with_discount(env, payer, merchant, asset, amount, order_ref, 0)
    }

    /// Settles a sale with an optional discount. Fees are calculated on the
    /// post-discount amount. `discount` is in smallest units; must not exceed `amount`.
    pub fn pay_with_discount(
        env: Env,
        payer: Address,
        merchant: Address,
        asset: Address,
        amount: i128,
        order_ref: String,
        discount: i128,
    ) -> Result<u64, PaymentError> {
        let config = Self::config(&env)?;
        if config.paused {
            return Err(PaymentError::Paused);
        }
        payer.require_auth();
        if discount < 0 || discount > amount {
            return Err(PaymentError::InvalidAmount);
        }
        let settlement = Self::settlement_for(&env, &config, &merchant, &asset)?;
        Self::settle(
            &env, &config, &payer, merchant, settlement, &asset, amount, order_ref, discount,
        )
    }

    /// Settles a payment using delegated authorization from the actual payer.
    /// The delegatee (caller) makes the payment on behalf of the delegator,
    /// deducting from their spending limit. Fails if delegation is expired
    /// or spending limit is exceeded.
    pub fn pay_delegated(
        env: Env,
        delegator: Address,
        merchant: Address,
        asset: Address,
        amount: i128,
        order_ref: String,
    ) -> Result<u64, PaymentError> {
        Self::pay_delegated_with_discount(env, delegator, merchant, asset, amount, order_ref, 0)
    }

    /// Settles a payment using delegated authorization with an optional discount.
    pub fn pay_delegated_with_discount(
        env: Env,
        delegator: Address,
        merchant: Address,
        asset: Address,
        amount: i128,
        order_ref: String,
        discount: i128,
    ) -> Result<u64, PaymentError> {
        let config = Self::config(&env)?;
        if config.paused {
            return Err(PaymentError::Paused);
        }
        if discount < 0 || discount > amount {
            return Err(PaymentError::InvalidAmount);
        }
        let delegatee = env.current_contract_address();
        let mut delegation = Self::get_delegation(&env, &delegator, &delegatee)?;

        if env.ledger().timestamp() > delegation.expiry {
            return Err(PaymentError::InvalidOrderRef);
        }
        if delegation.spent.checked_add(amount).ok_or(PaymentError::Overflow)?
            > delegation.spending_limit
        {
            return Err(PaymentError::InvalidAmount);
        }

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
            &Vec::new(&env),
        )
    }

    /// Settles a marketplace sale with a multi-party split: the platform fee
    /// goes to the treasury, each royalty recipient gets its `bps` share of
    /// the net amount, and the seller's settlement account gets the rest.
    /// Shares must be non-zero and add up to at most 100%. Like the platform
    /// fee, royalties are not clawed back on refunds.
    pub fn pay_with_royalties(
        env: Env,
        payer: Address,
        merchant: Address,
        asset: Address,
        amount: i128,
        order_ref: String,
        royalties: Vec<RoyaltyShare>,
    ) -> Result<u64, PaymentError> {
        let config = Self::config(&env)?;
        if config.paused {
            return Err(PaymentError::Paused);
        }
        if royalties.len() > MAX_ROYALTY_RECIPIENTS {
            return Err(PaymentError::InvalidRoyaltySplit);
        }
        let mut total_bps: u32 = 0;
        for share in royalties.iter() {
            if share.bps == 0 {
                return Err(PaymentError::InvalidRoyaltySplit);
            }
            total_bps = total_bps.saturating_add(share.bps);
        }
        if total_bps > BPS_DENOMINATOR {
            return Err(PaymentError::InvalidRoyaltySplit);
        }
        payer.require_auth();
        let settlement = Self::settlement_for(&env, &config, &merchant, &asset)?;
        Self::settle(
            &env, &config, &payer, merchant, settlement, &asset, amount, order_ref, &royalties,
        )
        let payment_id = Self::settle(
            &env, &config, &delegator, merchant, settlement, &asset, amount, order_ref, discount,
        )?;

        delegation.spent = delegation
            .spent
            .checked_add(amount)
            .ok_or(PaymentError::Overflow)?;
        Self::save_delegation(&env, &delegation);

        Ok(payment_id)
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
                line.discount,
            )?);
        }
        Ok(ids)

                &Vec::new(&env),
            )?);
        }
        Ok(ids)
        Self::with_reentrancy_guard(&env, || {
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
        })
 main
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
        let config = Self::config(&env)?;
        if config.paused {
            return Err(PaymentError::Paused);
        }
        let mut payment = Self::load(&env, payment_id)?;
        payment.settlement.require_auth();
        Self::with_reentrancy_guard(&env, || {
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
        })
    }

        let client = token::Client::new(&env, &payment.asset);
        Self::transfer_checked(&client, &payment.settlement, &payment.payer, amount)?;

        payment.refunded = total;
        payment.status = if total == payment.amount {
            PaymentStatus::Refunded
        } else {
            PaymentStatus::PartiallyRefunded
        };
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
        Ok(())
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
        Self::update(&env, |c| c.fee_bps = fee_bps)
    }

    /// Admin-only until governance is enabled; then use `propose_param_change`.
    pub fn set_treasury(env: Env, treasury: Address) -> Result<(), PaymentError> {
        Self::require_no_governance(&env)?;
        Self::update(&env, |c| c.treasury = treasury)
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
        Self::update(&env, |c| c.paused = true)
    }

    pub fn unpause(env: Env) -> Result<(), PaymentError> {
        Self::update(&env, |c| c.paused = false)
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
            return Err(PaymentError::InvalidOrderRef);
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

    // -------------------------------------------------------------------------
    // Recurring Payments
    // -------------------------------------------------------------------------

    /// Creates a recurring payment schedule. `interval` is in seconds, and
    /// `max_occurrences` is the maximum number of times the payment will occur.
    /// Pass `max_occurrences` as 0 for unlimited recurring payments.
    pub fn create_recurring(
        env: Env,
        payer: Address,
        merchant: Address,
        asset: Address,
        amount: i128,
        interval: u64,
        max_occurrences: u64,
    ) -> Result<u64, PaymentError> {
        let config = Self::config(&env)?;
        if config.paused {
            return Err(PaymentError::Paused);
        }
        payer.require_auth();
        if amount <= 0 {
            return Err(PaymentError::InvalidAmount);
        }
        if payer == &merchant {
            return Err(PaymentError::InvalidOrderRef);
        }
        if interval == 0 {
            return Err(PaymentError::InvalidOrderRef);
        }

        Self::settlement_for(&env, &config, &merchant, &asset)?;

        let recurring_count = env.storage().instance().get(&DataKey::RecurringCount).unwrap_or(0u64);
        let id = recurring_count.checked_add(1).ok_or(PaymentError::Overflow)?;

        let recurring = RecurringPayment {
            id,
            payer: payer.clone(),
            merchant: merchant.clone(),
            asset,
            amount,
            interval,
            max_occurrences,
            occurrences: 0,
            next_payment_time: env.ledger().timestamp().checked_add(interval).ok_or(PaymentError::Overflow)?,
            status: RecurringStatus::Active,
            created_at: env.ledger().timestamp(),
        };

        let storage = env.storage();
        storage.persistent().set(&DataKey::RecurringPayment(id), &recurring);
        extend_persistent(&env, &DataKey::RecurringPayment(id));
        storage.instance().set(&DataKey::RecurringCount, &id);
        extend_instance(&env);

        RecurringPaymentCreated {
            recurring_id: id,
            payer,
            merchant,
            amount,
            interval,
            max_occurrences,
        }
        .publish(&env);

        Ok(id)
    }

    /// Executes the next payment in a recurring schedule if it's due.
    /// Only the merchant or admin can trigger this.
    pub fn execute_recurring(
        env: Env,
        recurring_id: u64,
    ) -> Result<u64, PaymentError> {
        let config = Self::config(&env)?;
        if config.paused {
            return Err(PaymentError::Paused);
        }

        let mut recurring = Self::load_recurring(&env, recurring_id)?;
        if recurring.status != RecurringStatus::Active {
            return Err(PaymentError::InvalidOrderRef);
        }

        if env.ledger().timestamp() < recurring.next_payment_time {
            return Err(PaymentError::InvalidOrderRef);
        }

        if recurring.max_occurrences > 0 && recurring.occurrences >= recurring.max_occurrences {
            return Err(PaymentError::InvalidOrderRef);
        }

        recurring.merchant.require_auth();

        let settlement = Self::settlement_for(&env, &config, &recurring.merchant, &recurring.asset)?;
        let order_ref = String::from_slice(
            &env,
            format!("recurring-{}-{}", recurring_id, recurring.occurrences).as_bytes(),
        );

        let payment_id = Self::settle(
            &env,
            &config,
            &recurring.payer,
            recurring.merchant.clone(),
            settlement,
            &recurring.asset,
            recurring.amount,
            order_ref,
            0,
        )?;

        recurring.occurrences = recurring.occurrences.checked_add(1).ok_or(PaymentError::Overflow)?;
        recurring.next_payment_time = env.ledger().timestamp()
            .checked_add(recurring.interval)
            .ok_or(PaymentError::Overflow)?;

        if recurring.max_occurrences > 0 && recurring.occurrences >= recurring.max_occurrences {
            recurring.status = RecurringStatus::Completed;
        }

        Self::save_recurring(&env, &recurring);

        RecurringPaymentExecuted {
            recurring_id,
            payment_id,
            occurrence: recurring.occurrences,
        }
        .publish(&env);

        Ok(payment_id)
    }

    /// Cancels a recurring payment schedule.
    pub fn cancel_recurring(
        env: Env,
        recurring_id: u64,
    ) -> Result<(), PaymentError> {
        let mut recurring = Self::load_recurring(&env, recurring_id)?;
        recurring.payer.require_auth();

        if recurring.status == RecurringStatus::Cancelled {
            return Err(PaymentError::InvalidOrderRef);
        }

        recurring.status = RecurringStatus::Cancelled;
        Self::save_recurring(&env, &recurring);

        RecurringPaymentCancelled {
            recurring_id,
        }
        .publish(&env);

        Ok(())
    }

    /// Pauses a recurring payment schedule.
    pub fn pause_recurring(
        env: Env,
        recurring_id: u64,
    ) -> Result<(), PaymentError> {
        let mut recurring = Self::load_recurring(&env, recurring_id)?;
        recurring.payer.require_auth();

        if recurring.status != RecurringStatus::Active {
            return Err(PaymentError::InvalidOrderRef);
        }

        recurring.status = RecurringStatus::Paused;
        Self::save_recurring(&env, &recurring);
        Ok(())
    }

    /// Resumes a paused recurring payment schedule.
    pub fn resume_recurring(
        env: Env,
        recurring_id: u64,
    ) -> Result<(), PaymentError> {
        let mut recurring = Self::load_recurring(&env, recurring_id)?;
        recurring.payer.require_auth();

        if recurring.status != RecurringStatus::Paused {
            return Err(PaymentError::InvalidOrderRef);
        }

        recurring.status = RecurringStatus::Active;
        Self::save_recurring(&env, &recurring);
        Ok(())
    }

    /// Gets a recurring payment schedule.
    pub fn get_recurring(
        env: Env,
        recurring_id: u64,
    ) -> Result<RecurringPayment, PaymentError> {
        Self::load_recurring(&env, recurring_id)
    }

    /// Gets the total count of recurring payment schedules.
    pub fn recurring_count(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::RecurringCount)
            .unwrap_or(0)
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
    /// the pause flag and the payer's authorization. Fees are calculated on
    /// the post-discount amount.
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
        discount: i128,
    ) -> Result<u64, PaymentError> {
        if amount <= 0 {
            return Err(PaymentError::InvalidAmount);
        }
        if payer == &merchant {
            return Err(PaymentError::InvalidOrderRef);
        }
        if order_ref.is_empty() || order_ref.len() > MAX_ORDER_REF_LEN {
            return Err(PaymentError::InvalidOrderRef);
        }
        let order_key = DataKey::Order(merchant.clone(), order_ref.clone());
        if is_entry_valid(env, &order_key) {
            return Err(PaymentError::DuplicateOrder);
        }

        let discounted_amount = amount.checked_sub(discount).ok_or(PaymentError::InvalidAmount)?;
        let (fee, net) = split_fee(discounted_amount, config.fee_bps).ok_or(PaymentError::Overflow)?;
        let token = token::Client::new(env, asset);
        let mut seller_net = net;
        for share in royalties.iter() {
            let cut = bps_of(net, share.bps).ok_or(PaymentError::Overflow)?;
            if cut > 0 {
                token.transfer(payer, &share.recipient, &cut);
                seller_net -= cut;
                RoyaltyPaid {
                    payment_id: id,
                    recipient: share.recipient,
                    amount: cut,
                }
                .publish(env);
            }
        }
        if seller_net > 0 {
            token.transfer(payer, &settlement, &seller_net);
        }
        let client = token::Client::new(env, asset);
        Self::transfer_checked(&client, payer, &settlement, net)?;
        if fee > 0 {
            Self::transfer_checked(&client, payer, &config.treasury, fee)?;
        }

        let loyalty_points = match env.storage().instance().get::<_, Address>(&DataKey::Loyalty) {
            Some(loyalty) => LoyaltyClient::new(env, &loyalty).accrue(
                &env.current_contract_address(),
                &merchant,
                payer,
                &amount,
            ),
            None => 0,
        };

        let prev_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextPaymentId)
            .unwrap_or_else(|| Self::payment_count(env.clone()));
        let id = prev_id.checked_add(1).ok_or(PaymentError::Overflow)?;
        let active_count = Self::payment_count(env.clone())
            .checked_add(1)
            .ok_or(PaymentError::Overflow)?;
        let m_count = Self::merchant_payment_count(env.clone(), merchant.clone())
        let count = Self::payment_count(env.clone())
            .checked_add(1)
            .ok_or(PaymentError::Overflow)?;
        let id = Self::new_payment_id(env, payer, &merchant, &order_ref, count);
        let id = Self::payment_count(env.clone())
            .checked_add(1)
            .ok_or(PaymentError::Overflow)?;
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
            category: category.clone(),
            loyalty_points,
            status: PaymentStatus::Completed,
            created_at: env.ledger().timestamp(),
            chain_id: env.ledger().network_id(),
        };
        Self::save(env, &payment);
        let storage = env.storage();
        storage.persistent().set(&order_key, &id);
        extend_persistent(env, &order_key);
        let m_key = DataKey::MerchantPaymentCount(merchant.clone());
        env.storage().persistent().set(&m_key, &m_count);
        extend_persistent(env, &m_key);
        env.storage().instance().set(&DataKey::NextPaymentId, &id);
        env.storage()
            .instance()
            .set(&DataKey::PaymentCount, &active_count);
 stellar-fixes-consolidated
        storage.instance().set(&DataKey::PaymentCount, &id);

        env.storage().instance().set(&DataKey::PaymentCount, &count);
 main
        extend_instance(env);

        ComplianceRecord {
            kind: symbol_short!("payment"),
            merchant: merchant.clone(),
            payment_id: id,
            payer: payer.clone(),
            settlement: payment.settlement.clone(),
            asset: asset.clone(),
            amount,
            fee,
            order_ref: order_ref.clone(),
            timestamp: payment.created_at,
        }
        .publish(env);
        PaymentSettled {
            merchant: merchant.clone(),
            payment_id: id,
            payer: payer.clone(),
            asset: asset.clone(),
            amount,
            fee,
            order_ref,
            category,
            chain_id: env.ledger().network_id(),
        }
        .publish(env);

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

    /// The first 8 bytes of a SHA-256 over the payment's own data and the
    /// running count, so the next ID cannot be read off a counter. A
    /// collision (or zero) moves on to the next nonce.
    fn new_payment_id(
        env: &Env,
        payer: &Address,
        merchant: &Address,
        order_ref: &String,
        count: u64,
    ) -> u64 {
        let mut nonce: u32 = 0;
        loop {
            let seed = (
                merchant.clone(),
                order_ref.clone(),
                payer.clone(),
                count,
                env.ledger().sequence(),
                nonce,
            );
            let digest = env.crypto().sha256(&seed.to_xdr(env)).to_array();
            let mut head = [0u8; 8];
            head.copy_from_slice(&digest[..8]);
            let id = u64::from_be_bytes(head);
            if id != 0 && !env.storage().persistent().has(&DataKey::Payment(id)) {
                return id;
            }
            nonce += 1;
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

    fn config(env: &Env) -> Result<Config, PaymentError> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(PaymentError::NotInitialized)
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
        env: &Env,
        gov: &Governance,
        proposal: &mut ParamProposal,
    ) -> Result<bool, PaymentError> {
        if proposal.approvals.len() >= gov.threshold {
            let config = Self::config(env)?;
            match proposal.change.clone() {
                ParamChange::FeeBps(bps) => Self::store_config(env, config, |c| c.fee_bps = bps)?,
                ParamChange::Treasury(t) => Self::store_config(env, config, |c| c.treasury = t)?,
            }
            proposal.executed = true;
            ParamChangeApplied {
                proposal_id: proposal.id,
                change: proposal.change.clone(),
            }
            .publish(env);
        }
        let key = DataKey::Proposal(proposal.id);
        env.storage().persistent().set(&key, proposal);
        extend_persistent(env, &key);
        Ok(proposal.executed)
    /// Global monotonic counter in persistent storage, so event indices are
    /// never reissued across upgrades or payment restarts.
    fn next_event_index(env: &Env) -> u64 {
        let idx: u64 = env.storage().persistent().get(&DataKey::EventIndex).unwrap_or(0) + 1;
        env.storage().persistent().set(&DataKey::EventIndex, &idx);
        extend_persistent(env, &DataKey::EventIndex);
        idx
    }

    fn load(env: &Env, id: u64) -> Result<Payment, PaymentError> {
        let key = DataKey::Payment(id);
        if !is_entry_valid(env, &key) {
            return Err(PaymentError::ExpiredLedgerEntry);
        }
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
        if !is_entry_valid(env, &key) {
            return Err(PaymentError::PaymentNotFound);
        }
        let delegation = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(PaymentError::PaymentNotFound)?;
        extend_persistent(env, &key);
        Ok(delegation)
    }

    fn save_delegation(env: &Env, delegation: &Delegation) {
        let key = DataKey::Delegation(delegation.delegator.clone(), delegation.delegatee.clone());
        env.storage().persistent().set(&key, delegation);
        extend_persistent(env, &key);
    }

    fn load_recurring(env: &Env, id: u64) -> Result<RecurringPayment, PaymentError> {
        let key = DataKey::RecurringPayment(id);
        if !is_entry_valid(env, &key) {
            return Err(PaymentError::ExpiredLedgerEntry);
        }
        let recurring = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(PaymentError::PaymentNotFound)?;
        extend_persistent(env, &key);
        Ok(recurring)
    }

    fn save_recurring(env: &Env, recurring: &RecurringPayment) {
        let key = DataKey::RecurringPayment(recurring.id);
        env.storage().persistent().set(&key, recurring);
        extend_persistent(env, &key);
    }
}

#[cfg(test)]
mod test;
