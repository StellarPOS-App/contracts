extern crate std;

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::StellarAssetClient,
    Env,
};

pub(crate) const DAY: u64 = 24 * 60 * 60;

pub(crate) struct Setup<'a> {
    pub env: Env,
    pub client: EscrowServiceClient<'a>,
    pub token: token::Client<'a>,
    pub asset: Address,
    pub buyer: Address,
    pub seller: Address,
    pub arbiter: Address,
}

pub(crate) fn setup() -> Setup<'static> {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().with_mut(|l| l.timestamp = 1_000_000);

    let id = env.register(EscrowService, ());
    let client = EscrowServiceClient::new(&env, &id);
    client.initialize(&Address::generate(&env));

    let asset = env
        .register_stellar_asset_contract_v2(Address::generate(&env))
        .address();
    let buyer = Address::generate(&env);
    StellarAssetClient::new(&env, &asset).mint(&buyer, &10_000);

    Setup {
        token: token::Client::new(&env, &asset),
        client,
        asset,
        buyer,
        seller: Address::generate(&env),
        arbiter: Address::generate(&env),
        env,
    }
}

pub(crate) fn open(s: &Setup, amount: i128) -> u64 {
    let now = s.env.ledger().timestamp();
    s.client.create(
        &s.buyer,
        &s.seller,
        &s.arbiter,
        &s.asset,
        &amount,
        &(now + 3 * DAY),
        &(2 * DAY),
    )
}

#[test]
fn create_moves_funds_into_escrow() {
    let s = setup();
    let id = open(&s, 1_000);
    assert_eq!(s.token.balance(&s.buyer), 9_000);
    assert_eq!(s.token.balance(&s.client.address), 1_000);
    let e = s.client.get_escrow(&id);
    assert_eq!(e.status, EscrowStatus::Funded);
    assert_eq!(e.remaining(), 1_000);
}

#[test]
fn insured_escrow_funds_pool_and_covers_small_dispute_refund() {
    let s = setup();
    let now = s.env.ledger().timestamp();
    let id = s.client.create_insured(
        &s.buyer,
        &s.seller,
        &s.arbiter,
        &s.asset,
        &1_000,
        &(now + 3 * DAY),
        &(2 * DAY),
    );

    assert_eq!(s.client.insurance_balance(&s.asset), 10);
    assert_eq!(s.token.balance(&s.client.address), 1_010);
    s.client
        .raise_dispute(&s.buyer, &id, &DisputeReason::NonDelivery, &None);
    s.env.ledger().with_mut(|l| {
        l.timestamp = now + 3 * DAY;
        l.sequence_number = l
            .sequence_number
            .saturating_add((3 * DAY / LEDGER_CLOSE_SECONDS) as u32);
    });
    s.client.resolve_by_default(&id);

    assert_eq!(s.token.balance(&s.buyer), 9_990);
    assert_eq!(s.token.balance(&s.seller), 10);
    assert_eq!(s.client.insurance_balance(&s.asset), 0);
}

#[test]
fn gas_benchmarks_create_escrow_within_thresholds() {
    let s = setup();
    let now = s.env.ledger().timestamp();
    s.client.create(
        &s.buyer,
        &s.seller,
        &s.arbiter,
        &s.asset,
        &1_000,
        &(now + 3 * DAY),
        &(2 * DAY),
    );

    let budget = s.env.cost_estimate().budget();
    let cpu = budget.cpu_instruction_cost();
    let memory = budget.memory_bytes_cost();
    std::println!("create_escrow cpu={cpu} memory={memory}");
    assert!(cpu <= 50_000_000, "create_escrow CPU regression: {cpu}");
    assert!(
        memory <= 5_000_000,
        "create_escrow memory regression: {memory}"
    );
}

#[test]
fn buyer_release_pays_seller() {
    let s = setup();
    let id = open(&s, 1_000);
    s.env.ledger().with_mut(|l| l.timestamp += MIN_ESCROW_LOCK_PERIOD);
    s.client.release(&id);
    assert_eq!(s.token.balance(&s.seller), 1_000);
    assert_eq!(s.client.get_escrow(&id).status, EscrowStatus::Released);
    assert_eq!(
        s.client.try_release(&id),
        Err(Ok(EscrowError::InvalidStatus))
    );
}

#[test]
fn partial_releases_add_up() {
    let s = setup();
    let id = open(&s, 1_000);
    s.env.ledger().with_mut(|l| l.timestamp += MIN_ESCROW_LOCK_PERIOD);
    assert_eq!(s.client.release_partial(&id, &300), 700);
    assert_eq!(s.client.release_partial(&id, &700), 0);
    assert_eq!(s.token.balance(&s.seller), 1_000);
    assert_eq!(s.client.get_escrow(&id).status, EscrowStatus::Released);
}

#[test]
fn partial_release_cannot_exceed_remaining() {
    let s = setup();
    let id = open(&s, 1_000);
    s.env.ledger().with_mut(|l| l.timestamp += MIN_ESCROW_LOCK_PERIOD);
    s.client.release_partial(&id, &600);
    assert_eq!(
        s.client.try_release_partial(&id, &401),
        Err(Ok(EscrowError::InvalidAmount))
    );
}

#[test]
fn seller_claims_only_after_release_time() {
    let s = setup();
    let id = open(&s, 1_000);
    assert_eq!(s.client.try_claim(&id), Err(Ok(EscrowError::ReleaseNotDue)));
    s.env
        .ledger()
        .with_mut(|l| l.timestamp += 3 * DAY + SETTLEMENT_GRACE_PERIOD + 1);
    s.client.claim(&id);
    assert_eq!(s.token.balance(&s.seller), 1_000);
}

#[test]
fn seller_refund_returns_remainder_even_while_paused() {
    let s = setup();
    let id = open(&s, 1_000);
    s.env.ledger().with_mut(|l| l.timestamp += MIN_ESCROW_LOCK_PERIOD);
    s.client.release_partial(&id, &250);
    s.client.pause();
    s.client.refund(&id);
    assert_eq!(s.token.balance(&s.buyer), 9_750);
    assert_eq!(s.token.balance(&s.seller), 250);
    assert_eq!(s.client.get_escrow(&id).status, EscrowStatus::Refunded);
}

#[test]
fn pause_blocks_new_escrows() {
    let s = setup();
    s.client.pause();
    let now = s.env.ledger().timestamp();
    assert_eq!(
        s.client.try_create(
            &s.buyer,
            &s.seller,
            &s.arbiter,
            &s.asset,
            &1,
            &(now + DAY),
            &DAY
        ),
        Err(Ok(EscrowError::Paused))
    );
}

#[test]
fn creation_validation() {
    let s = setup();
    let now = s.env.ledger().timestamp();
    let c = &s.client;
    assert_eq!(
        c.try_create(
            &s.buyer,
            &s.seller,
            &s.arbiter,
            &s.asset,
            &0,
            &(now + DAY),
            &DAY
        ),
        Err(Ok(EscrowError::InvalidAmount))
    );
    assert_eq!(
        c.try_create(
            &s.buyer,
            &s.buyer,
            &s.arbiter,
            &s.asset,
            &1,
            &(now + DAY),
            &DAY
        ),
        Err(Ok(EscrowError::InvalidParties))
    );
    assert_eq!(
        c.try_create(
            &s.buyer,
            &s.seller,
            &s.seller,
            &s.asset,
            &1,
            &(now + DAY),
            &DAY
        ),
        Err(Ok(EscrowError::InvalidParties))
    );
    assert_eq!(
        c.try_create(&s.buyer, &s.seller, &s.arbiter, &s.asset, &1, &now, &DAY),
        Err(Ok(EscrowError::InvalidSchedule))
    );
    assert_eq!(
        c.try_create(
            &s.buyer,
            &s.seller,
            &s.arbiter,
            &s.asset,
            &1,
            &(now + DAY),
            &60
        ),
        Err(Ok(EscrowError::InvalidSchedule))
    );
}

#[test]
fn pay_direct_bypasses_escrow_when_both_parties_agree() {
    let s = setup();
    s.client.pay_direct(&s.buyer, &s.seller, &s.asset, &1_500);

    assert_eq!(s.token.balance(&s.buyer), 8_500);
    assert_eq!(s.token.balance(&s.seller), 1_500);
    // Contract neither custodies funds nor writes an escrow record
    assert_eq!(s.token.balance(&s.client.address), 0);
    assert_eq!(s.client.escrow_count(), 0);
}

#[test]
fn pay_direct_validates_inputs_and_respects_pause() {
    let s = setup();
    assert_eq!(
        s.client.try_pay_direct(&s.buyer, &s.seller, &s.asset, &0),
        Err(Ok(EscrowError::InvalidAmount))
    );
    assert_eq!(
        s.client.try_pay_direct(&s.buyer, &s.buyer, &s.asset, &100),
        Err(Ok(EscrowError::InvalidParties))
    );
    s.client.pause();
    assert_eq!(
        s.client.try_pay_direct(&s.buyer, &s.seller, &s.asset, &100),
        Err(Ok(EscrowError::Paused))
    );
}


#[test]
fn lock_has_a_floor_and_is_snapshotted_at_funding() {
    let s = setup();
    let c = &s.client;
    assert_eq!(c.min_lock_period(), MIN_ESCROW_LOCK_PERIOD);
    assert_eq!(
        c.try_set_min_lock_period(&(MIN_ESCROW_LOCK_PERIOD - 1)),
        Err(Ok(EscrowError::InvalidSchedule))
    );

    let lock = MIN_ESCROW_LOCK_PERIOD * 2;
    c.set_min_lock_period(&lock);
    let now = s.env.ledger().timestamp();
    assert_eq!(
        c.try_create(
            &s.buyer, &s.seller, &s.arbiter, &s.asset, &1_000,
            &(now + lock - 1), &DAY
        ),
        Err(Ok(EscrowError::InvalidSchedule))
    );

    let id = c.create(
        &s.buyer, &s.seller, &s.arbiter, &s.asset, &1_000,
        &(now + lock), &DAY,
    );
    c.set_min_lock_period(&MIN_ESCROW_LOCK_PERIOD);
    s.env.ledger().with_mut(|l| l.timestamp += lock - 1);
    assert_eq!(c.try_claim(&id), Err(Ok(EscrowError::ReleaseNotDue)));
    assert_eq!(c.try_release(&id), Err(Ok(EscrowError::ReleaseNotDue)));
    s.env.ledger().with_mut(|l| l.timestamp += 1 + SETTLEMENT_GRACE_PERIOD);
    c.claim(&id);
}

#[test]
fn each_escrow_pays_out_its_original_funding_asset() {
    let s = setup();
    let first = open(&s, 1_000);
    let second_asset = s.env.register_stellar_asset_contract_v2(Address::generate(&s.env)).address();
    StellarAssetClient::new(&s.env, &second_asset).mint(&s.buyer, &2_000);
    let now = s.env.ledger().timestamp();
    let second = s.client.create(
        &s.buyer, &s.seller, &s.arbiter, &second_asset, &2_000,
        &(now + DAY), &DAY,
    );
    s.env.ledger().with_mut(|l| l.timestamp += MIN_ESCROW_LOCK_PERIOD);
    s.client.release(&first);
    let second_token = token::Client::new(&s.env, &second_asset);
    assert_eq!(s.token.balance(&s.seller), 1_000);
    assert_eq!(second_token.balance(&s.seller), 0);
    s.client.refund(&second);
}
