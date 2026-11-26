extern crate std;

use super::*;
use loyalty_program::{LoyaltyProgram, LoyaltyProgramClient};
use merchant_registry::{MerchantRegistry, MerchantRegistryClient};
use soroban_sdk::{
    testutils::{Address as _, Events, Ledger},
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
    loyalty.enroll(&s.merchant, &10, &7);
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
        category: order_ref.clone(),
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


#[contract]
struct TestPriceOracle;

#[contractimpl]
impl TestPriceOracle {
    pub fn normalize(_env: Env, _asset: Address, amount: i128) -> i128 { amount * 2 }
    pub fn denormalize(_env: Env, _asset: Address, amount: i128) -> i128 { amount / 2 }
}

#[test]
fn oracle_normalization_is_used_for_asset_fee_quotes() {
    let s = setup(250);
    let oracle = s.env.register(TestPriceOracle, ());
    s.pp.set_price_oracle(&Some(oracle));
    let (fee, net) = s.pp.quote_in_asset(&s.asset, &(100 * UNIT));
    assert_eq!(fee, 25 * UNIT / 10);
    assert_eq!(fee + net, 100 * UNIT);
}

#[test]
fn batch_total_overflow_is_rejected_before_settlement() {
    let s = setup(0);
    let lines = vec![
        &s.env,
        BatchLine { merchant: s.merchant.clone(), amount: i128::MAX, order_ref: order(&s.env, "OVERFLOW-1"), category: order(&s.env, "batch") },
        BatchLine { merchant: s.merchant.clone(), amount: 1, order_ref: order(&s.env, "OVERFLOW-2"), category: order(&s.env, "batch") },
    ];
    assert_eq!(s.pp.try_pay_batch(&s.payer, &s.asset, &lines), Err(Ok(PaymentError::Overflow)));
}

#[test]
fn treasury_redirect_is_delayed_for_seven_days() {
    let s = setup(100);
    let next = Address::generate(&s.env);
    let now = s.env.ledger().timestamp();
    s.pp.set_treasury(&next);
    assert_eq!(s.pp.get_config().treasury, s.treasury);
    assert_eq!(s.pp.get_pending_treasury_change().unwrap().execute_after, now + TREASURY_CHANGE_DELAY);
    assert_eq!(s.pp.try_execute_treasury_change(), Err(Ok(PaymentError::TreasuryChangeNotReady)));
    s.env.ledger().with_mut(|l| l.timestamp = now + TREASURY_CHANGE_DELAY);
    s.pp.execute_treasury_change();
    assert_eq!(s.pp.get_config().treasury, next);
}


#[test]
fn admin_roles_default_to_legacy_admin_and_can_be_separated() {
    let s = setup(100);
    let initial = s.pp.get_admin_roles();
    assert_eq!(initial.fee_admin, initial.dispute_admin);
    assert_eq!(initial.fee_admin, initial.upgrade_admin);

    let fee_admin = Address::generate(&s.env);
    let dispute_admin = Address::generate(&s.env);
    let upgrade_admin = Address::generate(&s.env);
    s.pp.set_admin_roles(&fee_admin, &dispute_admin, &upgrade_admin);
    let roles = s.pp.get_admin_roles();
    assert_eq!(roles.fee_admin, fee_admin);
    assert_eq!(roles.dispute_admin, dispute_admin);
    assert_eq!(roles.upgrade_admin, upgrade_admin);
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
