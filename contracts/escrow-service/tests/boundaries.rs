//! Boundary checks for escrow timing, arbitration windows and insurance
//! premiums. Each test sits exactly on the second or value where a comparison
//! flips, so a mutated `<`, `<=`, `>` or range bound fails here.

use escrow_service::{
    dispute::DisputeReason, EscrowError, EscrowService, EscrowServiceClient,
    MAX_ARBITRATION_WINDOW, MIN_ARBITRATION_WINDOW,
};
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::{self, StellarAssetClient},
    Address, Env,
};

const DAY: u64 = 24 * 60 * 60;
const START: u64 = 1_000_000;

struct World<'a> {
    env: Env,
    escrow: EscrowServiceClient<'a>,
    token: token::Client<'a>,
    asset: Address,
    buyer: Address,
    seller: Address,
    arbiter: Address,
}

fn world(buyer_funds: i128) -> World<'static> {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().with_mut(|l| l.timestamp = START);

    let escrow = EscrowServiceClient::new(&env, &env.register(EscrowService, ()));
    escrow.initialize(&Address::generate(&env));

    let asset = env
        .register_stellar_asset_contract_v2(Address::generate(&env))
        .address();
    let buyer = Address::generate(&env);
    StellarAssetClient::new(&env, &asset).mint(&buyer, &buyer_funds);

    World {
        token: token::Client::new(&env, &asset),
        escrow,
        asset,
        buyer,
        seller: Address::generate(&env),
        arbiter: Address::generate(&env),
        env,
    }
}

fn advance(w: &World, secs: u64) {
    w.env.ledger().with_mut(|l| l.timestamp += secs);
}

#[test]
fn arbitration_window_accepts_exactly_its_limits() {
    let w = world(3_000);
    let release_at = START + 3 * DAY;

    assert_eq!(
        w.escrow.try_create(
            &w.buyer,
            &w.seller,
            &w.arbiter,
            &w.asset,
            &1_000,
            &release_at,
            &(MIN_ARBITRATION_WINDOW - 1),
        ),
        Err(Ok(EscrowError::InvalidSchedule))
    );
    assert!(w
        .escrow
        .try_create(
            &w.buyer,
            &w.seller,
            &w.arbiter,
            &w.asset,
            &1_000,
            &release_at,
            &MIN_ARBITRATION_WINDOW,
        )
        .is_ok());

    assert_eq!(
        w.escrow.try_create(
            &w.buyer,
            &w.seller,
            &w.arbiter,
            &w.asset,
            &1_000,
            &release_at,
            &(MAX_ARBITRATION_WINDOW + 1),
        ),
        Err(Ok(EscrowError::InvalidSchedule))
    );
    assert!(w
        .escrow
        .try_create(
            &w.buyer,
            &w.seller,
            &w.arbiter,
            &w.asset,
            &1_000,
            &release_at,
            &MAX_ARBITRATION_WINDOW,
        )
        .is_ok());
}

#[test]
fn ruling_is_allowed_up_to_the_deadline_and_default_waits_for_it() {
    let w = world(1_000);
    let id = w.escrow.create(
        &w.buyer,
        &w.seller,
        &w.arbiter,
        &w.asset,
        &1_000,
        &(START + 3 * DAY),
        &(2 * DAY),
        &(START + 33 * DAY), // expires_at: 30 days after release_at
    );
    w.escrow
        .raise_dispute(&w.buyer, &id, &DisputeReason::Defective, &None);

    // Raised at START, so the deadline is exactly START + 2 days.
    advance(&w, 2 * DAY);
    assert_eq!(
        w.escrow.try_resolve_by_default(&id),
        Err(Ok(EscrowError::DeadlineNotPassed))
    );
    w.escrow.resolve(&id, &3_000);
    assert_eq!(w.token.balance(&w.buyer), 300);
    assert_eq!(w.token.balance(&w.seller), 700);
}

#[test]
fn default_ruling_applies_one_second_after_the_deadline() {
    let w = world(1_000);
    let id = w.escrow.create(
        &w.buyer,
        &w.seller,
        &w.arbiter,
        &w.asset,
        &1_000,
        &(START + 3 * DAY),
        &(2 * DAY),
    );
    w.escrow
        .raise_dispute(&w.buyer, &id, &DisputeReason::NonDelivery, &None);

    advance(&w, 2 * DAY + 1);
    assert_eq!(
        w.escrow.try_resolve(&id, &5_000),
        Err(Ok(EscrowError::DeadlinePassed))
    );
    // Non-delivery defaults to a full refund for the buyer.
    assert_eq!(w.escrow.resolve_by_default(&id), 10_000);
    assert_eq!(w.token.balance(&w.buyer), 1_000);
    assert!(w.escrow.get_dispute(&id).by_default);
}

#[test]
fn insured_escrow_needs_a_premium_of_at_least_one_unit() {
    // The premium is 1% of the amount, rounded down, so 99 units pay none.
    let w = world(1_000);
    let release_at = START + 3 * DAY;

    assert_eq!(
        w.escrow.try_create_insured(
            &w.buyer,
            &w.seller,
            &w.arbiter,
            &w.asset,
            &99,
            &release_at,
            &(2 * DAY),
        ),
        Err(Ok(EscrowError::InvalidAmount))
    );
    assert_eq!(w.token.balance(&w.buyer), 1_000);

    // 100 units pay a premium of exactly 1, which lands in the insurance pool.
    w.escrow.create_insured(
        &w.buyer,
        &w.seller,
        &w.arbiter,
        &w.asset,
        &100,
        &release_at,
        &(2 * DAY),
    );
    assert_eq!(w.escrow.insurance_balance(&w.asset), 1);
    assert_eq!(w.token.balance(&w.buyer), 899);
}
