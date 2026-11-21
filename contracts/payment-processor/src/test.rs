extern crate std;

use super::*;
use loyalty_program::{LoyaltyProgram, LoyaltyProgramClient};
use merchant_registry::{MerchantRegistry, MerchantRegistryClient};
use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    token::StellarAssetClient,
    vec, Env, String,
    testutils::{Address as _, Events},
    token::StellarAssetClient,
    vec, Env, Event, String,
};

const UNIT: i128 = 10_000_000;

struct Setup<'a> {
    env: Env,
    pp: PaymentProcessorClient<'a>,
    registry: MerchantRegistryClient<'a>,
    token: token::Client<'a>,
    asset: Address,
    treasury: Address,
    merchant: Address,
    settlement: Address,
    payer: Address,
}

fn setup(fee_bps: u32) -> Setup<'static> {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let treasury = Address::generate(&env);
    let merchant = Address::generate(&env);
    let settlement = Address::generate(&env);
    let payer = Address::generate(&env);

    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let asset = sac.address();
    StellarAssetClient::new(&env, &asset).mint(&payer, &(1_000 * UNIT));

    let registry_id = env.register(MerchantRegistry, ());
    let registry = MerchantRegistryClient::new(&env, &registry_id);
    registry.initialize(&admin);
    registry.register(
        &merchant,
        &String::from_str(&env, "Lagos Bites"),
        &settlement,
        &vec![&env, asset.clone()],
    );
    registry.approve(&merchant);

    let pp_id = env.register(PaymentProcessor, ());
    let pp = PaymentProcessorClient::new(&env, &pp_id);
    pp.initialize(&admin, &registry_id, &treasury, &fee_bps);

    Setup {
        token: token::Client::new(&env, &asset),
        pp,
        registry,
        asset,
        treasury,
        merchant,
        settlement,
        payer,
        env,
    }
}

fn order(env: &Env, r: &str) -> String {
    String::from_str(env, r)
}

#[test]
fn payment_splits_fee_to_treasury_and_net_to_settlement() {
    let s = setup(250);
    let id = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(100 * UNIT),
        &order(&s.env, "ORD-1001"),
    );
    assert_eq!(s.pp.payment_count(), 1);

    assert_eq!(s.token.balance(&s.settlement), 975 * UNIT / 10);
    assert_eq!(s.token.balance(&s.treasury), 25 * UNIT / 10);
    assert_eq!(s.token.balance(&s.payer), 900 * UNIT);
    // The processor never custodies funds.
    assert_eq!(s.token.balance(&s.pp.address), 0);

    let p = s.pp.get_payment(&id);
    assert_eq!(p.fee, 25 * UNIT / 10);
    assert_eq!(p.status, PaymentStatus::Completed);
    assert_eq!(
        s.pp.payment_for_order(&s.merchant, &order(&s.env, "ORD-1001"))
            .id,
        id
    );
}

#[test]
fn zero_fee_skips_treasury_transfer() {
    let s = setup(0);
    s.pp.pay(&s.payer, &s.merchant, &s.asset, &UNIT, &order(&s.env, "A"));
    assert_eq!(s.token.balance(&s.settlement), UNIT);
    assert_eq!(s.token.balance(&s.treasury), 0);
}

#[test]
fn duplicate_order_ref_is_rejected() {
    let s = setup(100);
    s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &UNIT,
        &order(&s.env, "DUP"),
    );
    let again = s.pp.try_pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &UNIT,
        &order(&s.env, "DUP"),
    );
    assert_eq!(again, Err(Ok(PaymentError::DuplicateOrder)));
    assert_eq!(s.token.balance(&s.payer), 999 * UNIT);
}

#[test]
fn inactive_merchant_or_unlisted_asset_is_rejected() {
    let s = setup(100);
    s.registry.suspend(&s.merchant);
    assert_eq!(
        s.pp.try_pay(&s.payer, &s.merchant, &s.asset, &UNIT, &order(&s.env, "X")),
        Err(Ok(PaymentError::MerchantNotAccepting))
    );
    s.registry.reinstate(&s.merchant);

    let other = s
        .env
        .register_stellar_asset_contract_v2(Address::generate(&s.env))
        .address();
    assert_eq!(
        s.pp.try_pay(&s.payer, &s.merchant, &other, &UNIT, &order(&s.env, "Y")),
        Err(Ok(PaymentError::MerchantNotAccepting))
    );
}

#[test]
fn partial_then_full_refund() {
    let s = setup(0);
    let id = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(10 * UNIT),
        &order(&s.env, "R"),
    );

    let total = s.pp.refund(
        &id,
        &(4 * UNIT),
        &String::from_str(&s.env, "one item returned"),
    );
    assert_eq!(total, 4 * UNIT);
    assert_eq!(
        s.pp.get_payment(&id).status,
        PaymentStatus::PartiallyRefunded
    );

    s.pp.refund(&id, &(6 * UNIT), &String::from_str(&s.env, "rest"));
    assert_eq!(s.pp.get_payment(&id).status, PaymentStatus::Refunded);
    assert_eq!(s.token.balance(&s.payer), 1_000 * UNIT);
    assert_eq!(s.token.balance(&s.settlement), 0);

    assert_eq!(
        s.pp.try_refund(&id, &1, &String::from_str(&s.env, "again")),
        Err(Ok(PaymentError::AlreadyRefunded))
    );
}

#[test]
fn refund_cannot_exceed_gross_amount() {
    let s = setup(0);
    let id =
        s.pp.pay(&s.payer, &s.merchant, &s.asset, &UNIT, &order(&s.env, "Z"));
    assert_eq!(
        s.pp.try_refund(&id, &(UNIT + 1), &String::from_str(&s.env, "too much")),
        Err(Ok(PaymentError::RefundExceedsPayment))
    );
}

#[test]
fn payment_accrues_loyalty_when_enabled() {
    let s = setup(0);
    let loyalty_id = s.env.register(LoyaltyProgram, ());
    let loyalty = LoyaltyProgramClient::new(&s.env, &loyalty_id);
    loyalty.initialize(&Address::generate(&s.env));
    loyalty.set_processor(&s.pp.address);
    loyalty.enroll(&s.merchant, &10, &UNIT);
    s.pp.set_loyalty(&Some(loyalty_id));

    let id = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(20 * UNIT),
        &order(&s.env, "L"),
    );
    assert_eq!(s.pp.get_payment(&id).loyalty_points, 200);
    assert_eq!(loyalty.get_account(&s.merchant, &s.payer).points, 200);
}

#[test]
fn pause_blocks_payments() {
    let s = setup(0);
    s.pp.pause();
    assert_eq!(
        s.pp.try_pay(&s.payer, &s.merchant, &s.asset, &UNIT, &order(&s.env, "P")),
        Err(Ok(PaymentError::Paused))
    );
    s.pp.unpause();
    s.pp.pay(&s.payer, &s.merchant, &s.asset, &UNIT, &order(&s.env, "P"));
}

#[test]
fn fee_rate_is_capped() {
    let s = setup(0);
    assert_eq!(
        s.pp.try_set_fee_bps(&(MAX_FEE_BPS + 1)),
        Err(Ok(PaymentError::InvalidFeeRate))
    );
    s.pp.set_fee_bps(&MAX_FEE_BPS);
    assert_eq!(s.pp.quote(&(100 * UNIT)), (10 * UNIT, 90 * UNIT));
}

#[test]
fn input_validation() {
    let s = setup(0);
    assert_eq!(
        s.pp.try_pay(&s.payer, &s.merchant, &s.asset, &0, &order(&s.env, "V")),
        Err(Ok(PaymentError::InvalidAmount))
    );
    assert_eq!(
        s.pp.try_pay(&s.payer, &s.merchant, &s.asset, &UNIT, &order(&s.env, "")),
        Err(Ok(PaymentError::InvalidOrderRef))
    );
}

fn line(merchant: &Address, amount: i128, order_ref: String) -> BatchLine {
    BatchLine {
        merchant: merchant.clone(),
        amount,
        order_ref,
    }
}

/// A second approved merchant accepting the same asset.
fn second_merchant(s: &Setup) -> (Address, Address) {
    let merchant = Address::generate(&s.env);
    let settlement = Address::generate(&s.env);
    s.registry.register(
        &merchant,
        &String::from_str(&s.env, "Second Shop"),
        &settlement,
        &vec![&s.env, s.asset.clone()],
    );
    s.registry.approve(&merchant);
    (merchant, settlement)
}

#[test]
fn payment_ids_are_hashed_not_sequential() {
    let s = setup(0);
    let a = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &UNIT,
        &order(&s.env, "H-1"),
    );
    let b = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &UNIT,
        &order(&s.env, "H-2"),
    );
    assert_ne!(a, 1);
    assert_ne!(b, a + 1, "the next id cannot be read off the counter");
    assert_eq!(s.pp.payment_count(), 2);
    assert_eq!(s.pp.get_payment(&b).order_ref, order(&s.env, "H-2"));
    assert_eq!(
        s.pp.payment_for_order(&s.merchant, &order(&s.env, "H-1"))
            .id,
        a
    );
}

#[test]
fn every_sale_and_refund_emits_a_compliance_record() {
    let s = setup(250);
    let id = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(100 * UNIT),
        &order(&s.env, "C-1"),
    );
    let mut record = ComplianceRecord {
        kind: symbol_short!("payment"),
        merchant: s.merchant.clone(),
        payment_id: id,
        payer: s.payer.clone(),
        settlement: s.settlement.clone(),
        asset: s.asset.clone(),
        amount: 100 * UNIT,
        fee: 25 * UNIT / 10,
        order_ref: order(&s.env, "C-1"),
        timestamp: s.env.ledger().timestamp(),
    };
    let emitted = s.env.events().all().filter_by_contract(&s.pp.address);
    assert!(emitted
        .events()
        .contains(&record.to_xdr(&s.env, &s.pp.address)));

    s.pp.refund(&id, &(10 * UNIT), &String::from_str(&s.env, "returned"));
    record.kind = symbol_short!("refund");
    record.amount = 10 * UNIT;
    record.fee = 0;
    let emitted = s.env.events().all().filter_by_contract(&s.pp.address);
    assert!(emitted
        .events()
        .contains(&record.to_xdr(&s.env, &s.pp.address)));
}

/// A token whose `transfer` reports success but moves nothing.
#[contract]
pub struct LyingToken;

#[contractimpl]
impl LyingToken {
    pub fn transfer(_env: Env, _from: Address, _to: Address, _amount: i128) {}

    pub fn balance(_env: Env, _id: Address) -> i128 {
        0
    }
}

#[test]
fn a_token_that_does_not_move_funds_is_rejected() {
    let s = setup(100);
    let fake = s.env.register(LyingToken, ());
    let merchant = Address::generate(&s.env);
    s.registry.register(
        &merchant,
        &String::from_str(&s.env, "Fake Shop"),
        &Address::generate(&s.env),
        &vec![&s.env, fake.clone()],
    );
    s.registry.approve(&merchant);

    assert_eq!(
        s.pp.try_pay(&s.payer, &merchant, &fake, &UNIT, &order(&s.env, "F-1")),
        Err(Ok(PaymentError::TransferFailed))
    );
    assert_eq!(s.pp.payment_count(), 0);
    assert_eq!(
        s.pp.try_payment_for_order(&merchant, &order(&s.env, "F-1")),
        Err(Ok(PaymentError::PaymentNotFound))
    );
}

#[test]
fn batch_settles_every_line_across_merchants() {
    let s = setup(100);
    let (m2, settle2) = second_merchant(&s);
    let ids = s.pp.pay_batch(
        &s.payer,
        &s.asset,
        &vec![
            &s.env,
            line(&s.merchant, 10 * UNIT, order(&s.env, "TAB-1")),
            line(&m2, 20 * UNIT, order(&s.env, "INV-9")),
            line(&s.merchant, 30 * UNIT, order(&s.env, "TAB-2")),
        ],
    );
    assert_eq!(ids.len(), 3);
    assert_eq!(s.pp.payment_count(), 3);
    for (i, r) in ["TAB-1", "INV-9", "TAB-2"].iter().enumerate() {
        let id = ids.get(i as u32).unwrap();
        assert_eq!(s.pp.get_payment(&id).order_ref, order(&s.env, r));
    }
    // 1% fee per line, net to each merchant's settlement account.
    assert_eq!(s.token.balance(&s.settlement), 396 * UNIT / 10);
    assert_eq!(s.token.balance(&settle2), 198 * UNIT / 10);
    assert_eq!(s.token.balance(&s.treasury), 6 * UNIT / 10);
    assert_eq!(s.token.balance(&s.payer), 940 * UNIT);
    assert_eq!(
        s.pp.payment_for_order(&m2, &order(&s.env, "INV-9")).amount,
        20 * UNIT
    );
}

#[test]
fn batch_is_all_or_nothing() {
    let s = setup(100);
    let stranger = Address::generate(&s.env);
    let res = s.pp.try_pay_batch(
        &s.payer,
        &s.asset,
        &vec![
            &s.env,
            line(&s.merchant, 10 * UNIT, order(&s.env, "OK-1")),
            line(&stranger, 5 * UNIT, order(&s.env, "BAD-1")),
        ],
    );
    assert_eq!(res, Err(Ok(PaymentError::MerchantNotAccepting)));
    // The valid first line was rolled back too.
    assert_eq!(s.token.balance(&s.payer), 1_000 * UNIT);
    assert_eq!(s.pp.payment_count(), 0);
    assert_eq!(
        s.pp.try_payment_for_order(&s.merchant, &order(&s.env, "OK-1")),
        Err(Ok(PaymentError::PaymentNotFound))
    );
}

#[test]
fn batch_rejects_a_repeated_order_inside_itself() {
    let s = setup(0);
    let res = s.pp.try_pay_batch(
        &s.payer,
        &s.asset,
        &vec![
            &s.env,
            line(&s.merchant, UNIT, order(&s.env, "SAME")),
            line(&s.merchant, UNIT, order(&s.env, "SAME")),
        ],
    );
    assert_eq!(res, Err(Ok(PaymentError::DuplicateOrder)));
    assert_eq!(s.token.balance(&s.payer), 1_000 * UNIT);
}

#[test]
fn batch_size_limits_and_pause() {
    let s = setup(0);
    assert_eq!(
        s.pp.try_pay_batch(&s.payer, &s.asset, &vec![&s.env]),
        Err(Ok(PaymentError::InvalidBatch))
    );
    let mut big = vec![&s.env];
    for i in 0..=MAX_BATCH {
        big.push_back(line(
            &s.merchant,
            UNIT,
            String::from_str(&s.env, &std::format!("B-{i}")),
        ));
    }
    assert_eq!(
        s.pp.try_pay_batch(&s.payer, &s.asset, &big),
        Err(Ok(PaymentError::InvalidBatch))
    );
    s.pp.pause();
    assert_eq!(
        s.pp.try_pay_batch(
            &s.payer,
            &s.asset,
            &vec![&s.env, line(&s.merchant, UNIT, order(&s.env, "P"))]
        ),
        Err(Ok(PaymentError::Paused))
    );
}

// ---------------------------------------------------------------------------
// Royalty splits
// ---------------------------------------------------------------------------

fn share(recipient: &Address, bps: u32) -> RoyaltyShare {
    RoyaltyShare {
        recipient: recipient.clone(),
        bps,
    }
}

#[test]
fn royalty_split_pays_platform_royalties_and_seller() {
    let s = setup(250);
    let artist = Address::generate(&s.env);
    let curator = Address::generate(&s.env);
    s.pp.pay_with_royalties(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(100 * UNIT),
        &order(&s.env, "NFT-1"),
        &vec![&s.env, share(&artist, 1_000), share(&curator, 500)],
    );
    // fee 2.5 to treasury; net 97.5 -> artist 10%, curator 5%, seller the rest
    assert_eq!(s.token.balance(&s.treasury), 25 * UNIT / 10);
    assert_eq!(s.token.balance(&artist), 975 * UNIT / 100);
    assert_eq!(s.token.balance(&curator), 4_875 * UNIT / 1_000);
    assert_eq!(s.token.balance(&s.settlement), 828_750 * UNIT / 10_000);
    assert_eq!(s.token.balance(&s.payer), 900 * UNIT);
}

#[test]
fn royalty_split_rejects_zero_shares_and_more_than_one_hundred_percent() {
    let s = setup(0);
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);
    let pay = |r: &str, shares: Vec<RoyaltyShare>| {
        s.pp.try_pay_with_royalties(
            &s.payer,
            &s.merchant,
            &s.asset,
            &UNIT,
            &order(&s.env, r),
            &shares,
        )
    };
    assert_eq!(
        pay("A", vec![&s.env, share(&a, 0)]),
        Err(Ok(PaymentError::InvalidRoyaltySplit))
    );
    assert_eq!(
        pay("B", vec![&s.env, share(&a, 6_000), share(&b, 4_001)]),
        Err(Ok(PaymentError::InvalidRoyaltySplit))
    );
    // exactly 100% leaves nothing for the seller but is allowed
    pay("C", vec![&s.env, share(&a, 6_000), share(&b, 4_000)])
        .unwrap()
        .unwrap();
    assert_eq!(s.token.balance(&s.settlement), 0);
    assert_eq!(s.token.balance(&a) + s.token.balance(&b), UNIT);
}

#[test]
fn royalty_split_caps_the_number_of_recipients() {
    let s = setup(0);
    let mut shares = Vec::new(&s.env);
    for _ in 0..=MAX_ROYALTY_RECIPIENTS {
        shares.push_back(share(&Address::generate(&s.env), 100));
    }
    assert_eq!(
        s.pp.try_pay_with_royalties(
            &s.payer,
            &s.merchant,
            &s.asset,
            &UNIT,
            &order(&s.env, "X"),
            &shares
        ),
        Err(Ok(PaymentError::InvalidRoyaltySplit))
    );
}

// ---------------------------------------------------------------------------
// Governance
// ---------------------------------------------------------------------------

#[test]
fn governance_moves_parameter_changes_to_council_votes() {
    let s = setup(250);
    let (a, b, c) = (
        Address::generate(&s.env),
        Address::generate(&s.env),
        Address::generate(&s.env),
    );
    s.pp.enable_governance(&vec![&s.env, a.clone(), b.clone(), c.clone()], &2);

    // the admin can no longer change critical parameters alone
    assert_eq!(
        s.pp.try_set_fee_bps(&100),
        Err(Ok(PaymentError::GovernanceActive))
    );
    assert_eq!(
        s.pp.try_set_treasury(&a),
        Err(Ok(PaymentError::GovernanceActive))
    );

    let id = s.pp.propose_param_change(&a, &ParamChange::FeeBps(100));
    assert_eq!(s.pp.get_config().fee_bps, 250, "one approval is not enough");
    assert_eq!(
        s.pp.try_approve_param_change(&a, &id),
        Err(Ok(PaymentError::AlreadyApproved))
    );
    assert!(s.pp.approve_param_change(&b, &id));
    assert_eq!(s.pp.get_config().fee_bps, 100);
    assert!(s.pp.get_param_proposal(&id).executed);
    assert_eq!(
        s.pp.try_approve_param_change(&c, &id),
        Err(Ok(PaymentError::ProposalExecuted))
    );

    let outsider = Address::generate(&s.env);
    assert_eq!(
        s.pp.try_propose_param_change(&outsider, &ParamChange::Treasury(outsider.clone())),
        Err(Ok(PaymentError::NotCouncilMember))
    );
    assert_eq!(
        s.pp.try_propose_param_change(&a, &ParamChange::FeeBps(MAX_FEE_BPS + 1)),
        Err(Ok(PaymentError::InvalidFeeRate))
    );
}

#[test]
fn governance_validates_the_council() {
    let s = setup(250);
    let a = Address::generate(&s.env);
    assert_eq!(
        s.pp.try_enable_governance(&vec![&s.env, a.clone()], &2),
        Err(Ok(PaymentError::InvalidGovernance))
    );
    assert_eq!(
        s.pp.try_enable_governance(&vec![&s.env, a.clone(), a.clone()], &1),
        Err(Ok(PaymentError::InvalidGovernance))
    );
    s.pp.enable_governance(&vec![&s.env, a.clone()], &1);
    assert_eq!(
        s.pp.try_enable_governance(&vec![&s.env, a.clone()], &1),
        Err(Ok(PaymentError::GovernanceActive))
    );

    // a threshold of one applies the proposal immediately
    s.pp.propose_param_change(&a, &ParamChange::Treasury(a.clone()));
    assert_eq!(s.pp.get_config().treasury, a);
#[test]
fn pause_blocks_refunds() {
    let s = setup(0);
    let id = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(10 * UNIT),
        &order(&s.env, "PR-1"),
    );

    s.pp.pause();
    assert_eq!(
        s.pp.try_refund(&id, &(5 * UNIT), &String::from_str(&s.env, "paused")),
        Err(Ok(PaymentError::Paused))
    );
    assert_eq!(s.pp.get_payment(&id).refunded, 0);
    assert_eq!(s.pp.get_payment(&id).status, PaymentStatus::Completed);

    s.pp.unpause();
    let total =
        s.pp.refund(&id, &(5 * UNIT), &String::from_str(&s.env, "resumed"));
    assert_eq!(total, 5 * UNIT);
    assert_eq!(
        s.pp.get_payment(&id).status,
        PaymentStatus::PartiallyRefunded
    );
}

#[test]
fn refund_decrements_payment_counts_without_reusing_ids() {
    let s = setup(0);
    assert_eq!(s.pp.payment_count(), 0);
    assert_eq!(s.pp.merchant_payment_count(&s.merchant), 0);

    let id1 = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(10 * UNIT),
        &order(&s.env, "CNT-1"),
    );
    assert_eq!(id1, 1);
    assert_eq!(s.pp.payment_count(), 1);
    assert_eq!(s.pp.merchant_payment_count(&s.merchant), 1);

    // Partial refund leaves the payment active (count unchanged).
    s.pp.refund(&id1, &(4 * UNIT), &String::from_str(&s.env, "partial"));
    assert_eq!(s.pp.payment_count(), 1);
    assert_eq!(s.pp.merchant_payment_count(&s.merchant), 1);

    // Completing the refund decrements both global and merchant active counts once.
    s.pp.refund(&id1, &(6 * UNIT), &String::from_str(&s.env, "full"));
    assert_eq!(s.pp.payment_count(), 0);
    assert_eq!(s.pp.merchant_payment_count(&s.merchant), 0);

    // Next payment gets a fresh monotonic ID and does not overwrite payment #1.
    let id2 = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(5 * UNIT),
        &order(&s.env, "CNT-2"),
    );
    assert_eq!(id2, 2);
    assert_eq!(s.pp.payment_count(), 1);
    assert_eq!(s.pp.merchant_payment_count(&s.merchant), 1);
    assert_eq!(s.pp.get_payment(&id1).status, PaymentStatus::Refunded);
    assert_eq!(s.pp.get_payment(&id2).status, PaymentStatus::Completed);
}

#[derive(Debug, PartialEq, Eq)]
struct StateSnapshot {
    paused: bool,
    fee_bps: u32,
    payment_count: u64,
    merchant_payment_count: u64,
    payer_balance: i128,
    settlement_balance: i128,
    treasury_balance: i128,
    processor_balance: i128,
}

fn capture_state(s: &Setup) -> StateSnapshot {
    let cfg = s.pp.get_config();
    StateSnapshot {
        paused: cfg.paused,
        fee_bps: cfg.fee_bps,
        payment_count: s.pp.payment_count(),
        merchant_payment_count: s.pp.merchant_payment_count(&s.merchant),
        payer_balance: s.token.balance(&s.payer),
        settlement_balance: s.token.balance(&s.settlement),
        treasury_balance: s.token.balance(&s.treasury),
        processor_balance: s.token.balance(&s.pp.address),
fn split_payment_distributes_across_contributors() {
    let s = setup(200); // 2% fee
    let payer2 = Address::generate(&s.env);
    let payer3 = Address::generate(&s.env);
    StellarAssetClient::new(&s.env, &s.asset).mint(&payer2, &(500 * UNIT));
    StellarAssetClient::new(&s.env, &s.asset).mint(&payer3, &(500 * UNIT));

    let shares = vec![
        &s.env,
        SplitShare {
            payer: s.payer.clone(),
            amount: 50 * UNIT,
        },
        SplitShare {
            payer: payer2.clone(),
            amount: 30 * UNIT,
        },
        SplitShare {
            payer: payer3.clone(),
            amount: 20 * UNIT,
        },
    ];

    let id =
        s.pp.split_payment(&s.merchant, &s.asset, &shares, &order(&s.env, "GRP-1"));
    assert_eq!(id, 1);

    assert_eq!(s.token.balance(&s.payer), 950 * UNIT);
    assert_eq!(s.token.balance(&payer2), 470 * UNIT);
    assert_eq!(s.token.balance(&payer3), 480 * UNIT);
    assert_eq!(s.token.balance(&s.settlement), 98 * UNIT);
    assert_eq!(s.token.balance(&s.treasury), 2 * UNIT);
    assert_eq!(s.token.balance(&s.pp.address), 0);

    let p = s.pp.get_payment(&id);
    assert_eq!(p.amount, 100 * UNIT);
    assert_eq!(p.fee, 2 * UNIT);
    assert_eq!(p.status, PaymentStatus::Completed);

    // Duplicate order ref is rejected
    assert_eq!(
        s.pp.try_split_payment(&s.merchant, &s.asset, &shares, &order(&s.env, "GRP-1")),
        Err(Ok(PaymentError::DuplicateOrder))
    );
    // Empty shares rejected
    assert_eq!(
        s.pp.try_split_payment(
            &s.merchant,
            &s.asset,
            &vec![&s.env],
            &order(&s.env, "GRP-2")
        ),
        Err(Ok(PaymentError::InvalidSplit))
    );
    // Non-positive share rejected
    assert_eq!(
        s.pp.try_split_payment(
            &s.merchant,
            &s.asset,
            &vec![
                &s.env,
                SplitShare {
                    payer: s.payer.clone(),
                    amount: 0
                }
            ],
            &order(&s.env, "GRP-3"),
        ),
        Err(Ok(PaymentError::InvalidAmount))
    );
}

#[contract]
struct MaliciousCallback;

#[contracttype]
enum CallbackKey {
    Target,
    ReentryBlocked,
}

#[contractimpl]
impl MaliciousCallback {
    pub fn init(env: Env, target: Address) {
        env.storage().instance().set(&CallbackKey::Target, &target);
        env.storage()
            .instance()
            .set(&CallbackKey::ReentryBlocked, &false);
    }

    pub fn on_payment_settled(
        env: Env,
        _payment_id: u64,
        merchant: Address,
        payer: Address,
        asset: Address,
        amount: i128,
    ) {
        let target: Address = env.storage().instance().get(&CallbackKey::Target).unwrap();
        let res = PaymentProcessorClient::new(&env, &target).try_pay(
            &payer,
            &merchant,
            &asset,
            &amount,
            &String::from_str(&env, "REENTER"),
        );
        if res.is_err() {
            env.storage()
                .instance()
                .set(&CallbackKey::ReentryBlocked, &true);
        }
    }

    pub fn reentry_blocked(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&CallbackKey::ReentryBlocked)
            .unwrap_or(false)
    }
}

#[test]
fn state_snapshot_lifecycle_pay_refund_pause_and_next_pay() {
    let s = setup(250);
    s.env.ledger().set_timestamp(1_700_000_000);
    // Top up settlement so it can absorb the 2.5% protocol fee on a full refund.
    StellarAssetClient::new(&s.env, &s.asset).mint(&s.settlement, &(10 * UNIT));

    assert_eq!(
        capture_state(&s),
        StateSnapshot {
            paused: false,
            fee_bps: 250,
            payment_count: 0,
            merchant_payment_count: 0,
            payer_balance: 1_000 * UNIT,
            settlement_balance: 10 * UNIT,
            treasury_balance: 0,
            processor_balance: 0,
        }
    );

    // 1. Settle payment #1 (100 UNIT -> 2.5 fee, 97.5 net).
    let id1 = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(100 * UNIT),
        &order(&s.env, "SNAP-1"),
    );
    assert_eq!(id1, 1);
    assert_eq!(
        capture_state(&s),
        StateSnapshot {
            paused: false,
            fee_bps: 250,
            payment_count: 1,
            merchant_payment_count: 1,
            payer_balance: 900 * UNIT,
            settlement_balance: 1_075 * UNIT / 10,
            treasury_balance: 25 * UNIT / 10,
            processor_balance: 0,
        }
    );
    assert_eq!(
        s.pp.get_payment(&id1),
        Payment {
            id: 1,
            payer: s.payer.clone(),
            merchant: s.merchant.clone(),
            settlement: s.settlement.clone(),
            asset: s.asset.clone(),
            amount: 100 * UNIT,
            fee: 25 * UNIT / 10,
            refunded: 0,
            order_ref: order(&s.env, "SNAP-1"),
            loyalty_points: 0,
            status: PaymentStatus::Completed,
            created_at: 1_700_000_000,
        }
    );

    // 2. Partial refund (40 UNIT).
    s.pp.refund(&id1, &(40 * UNIT), &String::from_str(&s.env, "partial"));
    assert_eq!(
        capture_state(&s),
        StateSnapshot {
            paused: false,
            fee_bps: 250,
            payment_count: 1,
            merchant_payment_count: 1,
            payer_balance: 940 * UNIT,
            settlement_balance: 675 * UNIT / 10,
            treasury_balance: 25 * UNIT / 10,
            processor_balance: 0,
        }
    );
    assert_eq!(
        s.pp.get_payment(&id1).status,
        PaymentStatus::PartiallyRefunded
    );
    assert_eq!(s.pp.get_payment(&id1).refunded, 40 * UNIT);

    // 3. Pause blocks both pay and refund without mutating state.
    s.pp.pause();
    let paused_snap = capture_state(&s);
    assert!(paused_snap.paused);
fn reentrancy_via_merchant_callback_is_blocked() {
    let s = setup(100);
    let cb_id = s.env.register(MaliciousCallback, ());
    let cb = MaliciousCallbackClient::new(&s.env, &cb_id);
    cb.init(&s.pp.address);

    s.pp.set_merchant_callback(&s.merchant, &Some(cb_id.clone()));
    assert_eq!(s.pp.get_merchant_callback(&s.merchant), Some(cb_id));

    let id = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(10 * UNIT),
        &order(&s.env, "ORD-CB-1"),
    );
    assert_eq!(id, 1);
    assert!(cb.reentry_blocked());
    // Re-entrant payment was never recorded
    assert_eq!(s.pp.payment_count(), 1);

    // Lock is released after completion, so a subsequent payment succeeds
    let id2 = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(10 * UNIT),
        &order(&s.env, "ORD-CB-2"),
    );
    assert_eq!(id2, 2);

    // Explicit reentrancy lock rejects pay, split_payment, and refund
    s.env.as_contract(&s.pp.address, || {
        s.env
            .storage()
            .instance()
            .set(&DataKey::ReentrancyLock, &true);
    });
    assert_eq!(
        s.pp.try_pay(
            &s.payer,
            &s.merchant,
            &s.asset,
            &UNIT,
            &order(&s.env, "BLOCKED")
        ),
        Err(Ok(PaymentError::Paused))
    );
    assert_eq!(
        s.pp.try_refund(&id1, &(60 * UNIT), &String::from_str(&s.env, "blocked")),
        Err(Ok(PaymentError::Paused))
    );
    assert_eq!(capture_state(&s), paused_snap);

    // 4. Unpause and complete refund (60 UNIT) -> counts drop to 0.
    s.pp.unpause();
    s.pp.refund(&id1, &(60 * UNIT), &String::from_str(&s.env, "final"));
    assert_eq!(
        capture_state(&s),
        StateSnapshot {
            paused: false,
            fee_bps: 250,
            payment_count: 0,
            merchant_payment_count: 0,
            payer_balance: 1_000 * UNIT,
            settlement_balance: 75 * UNIT / 10,
            treasury_balance: 25 * UNIT / 10,
            processor_balance: 0,
        }
    );
    assert_eq!(s.pp.get_payment(&id1).status, PaymentStatus::Refunded);
    assert_eq!(s.pp.get_payment(&id1).refunded, 100 * UNIT);

    // 5. Subsequent payment gets id #2 while #1 remains Refunded.
    s.env.ledger().set_timestamp(1_700_000_060);
    let id2 = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(40 * UNIT),
        &order(&s.env, "SNAP-2"),
    );
    assert_eq!(id2, 2);
    assert_eq!(
        capture_state(&s),
        StateSnapshot {
            paused: false,
            fee_bps: 250,
            payment_count: 1,
            merchant_payment_count: 1,
            payer_balance: 960 * UNIT,
            settlement_balance: 465 * UNIT / 10,
            treasury_balance: 35 * UNIT / 10,
            processor_balance: 0,
        }
    );

    let ledger_snap = s.env.to_snapshot();
    assert_eq!(ledger_snap.ledger.timestamp, 1_700_000_060);
    assert!(!ledger_snap.ledger.ledger_entries.is_empty());
}

#[test]
fn state_snapshot_batch_and_per_merchant_refund_counts() {
    let s = setup(100);
    let (m2, _settle2) = second_merchant(&s);
    StellarAssetClient::new(&s.env, &s.asset).mint(&s.settlement, &(5 * UNIT));

    let ids = s.pp.pay_batch(
        &s.payer,
        &s.asset,
        &vec![
            &s.env,
            line(&s.merchant, 10 * UNIT, order(&s.env, "B-1")),
            line(&m2, 20 * UNIT, order(&s.env, "B-2")),
            line(&s.merchant, 30 * UNIT, order(&s.env, "B-3")),
        ],
    );
    assert_eq!(ids, vec![&s.env, 1u64, 2, 3]);
    assert_eq!(s.pp.payment_count(), 3);
    assert_eq!(s.pp.merchant_payment_count(&s.merchant), 2);
    assert_eq!(s.pp.merchant_payment_count(&m2), 1);

    // Fully refunding merchant 1's first payment decrements only merchant 1 and global count.
    s.pp.refund(&1, &(10 * UNIT), &String::from_str(&s.env, "cancel item"));
    assert_eq!(s.pp.payment_count(), 2);
    assert_eq!(s.pp.merchant_payment_count(&s.merchant), 1);
    assert_eq!(s.pp.merchant_payment_count(&m2), 1);

    // Config transition snapshot.
    s.pp.set_fee_bps(&500);
    let cfg = s.pp.get_config();
    assert_eq!(cfg.fee_bps, 500);
    assert!(!cfg.paused);
            &order(&s.env, "LOCKED"),
        ),
        Err(Ok(PaymentError::Reentrancy))
    );
    assert_eq!(
        s.pp.try_refund(&id, &UNIT, &order(&s.env, "LOCKED")),
        Err(Ok(PaymentError::Reentrancy))
    );
}

#[test]
fn registered_merchant_payment_count_reads_zero_without_allocating_storage() {
    let s = setup(250);
    let key = DataKey::MerchantPaymentCount(s.merchant.clone());
    assert!(!s.env.as_contract(&s.pp.address, || {
        s.env.storage().persistent().has(&key)
    }));
    assert_eq!(s.pp.merchant_payment_count(&s.merchant), 0);
    assert!(!s.env.as_contract(&s.pp.address, || {
        s.env.storage().persistent().has(&key)
    }));
}
