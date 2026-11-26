//! End-to-end dispute scenarios against the public contract interface.

use escrow_service::{
    dispute::DisputeReason, EscrowError, EscrowService, EscrowServiceClient, EscrowStatus,
    CURRENT_SCHEMA_VERSION, LEDGER_CLOSE_SECONDS, MAX_EVIDENCE, MIN_ESCROW_LOCK_PERIOD, SETTLEMENT_GRACE_PERIOD,
};
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::{self, StellarAssetClient},
    Address, BytesN, Env,
};

const DAY: u64 = 24 * 60 * 60;

struct World<'a> {
    env: Env,
    escrow: EscrowServiceClient<'a>,
    token: token::Client<'a>,
    buyer: Address,
    seller: Address,
    arbiter: Address,
    id: u64,
}

/// A funded 1,000-unit escrow: auto-release in 3 days, arbiter has 2 days
/// to rule on any dispute.
fn world() -> World<'static> {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().with_mut(|l| l.timestamp = 1_000_000);

    let escrow = EscrowServiceClient::new(&env, &env.register(EscrowService, ()));
    escrow.initialize(&Address::generate(&env));

    let asset = env
        .register_stellar_asset_contract_v2(Address::generate(&env))
        .address();
    let buyer = Address::generate(&env);
    let seller = Address::generate(&env);
    let arbiter = Address::generate(&env);
    StellarAssetClient::new(&env, &asset).mint(&buyer, &1_000);

    let id = escrow.create(
        &buyer,
        &seller,
        &arbiter,
        &asset,
        &1_000,
        &(1_000_000 + 3 * DAY),
        &(2 * DAY),
        &(1_000_000 + 33 * DAY), // expires_at: 30 days after release_at
    );
    World {
        token: token::Client::new(&env, &asset),
        env,
        escrow,
        buyer,
        seller,
        arbiter,
        id,
    }
}

fn hash(env: &Env, byte: u8) -> BytesN<32> {
    BytesN::from_array(env, &[byte; 32])
}

fn advance(w: &World, secs: u64) {
    w.env.ledger().with_mut(|l| {
        l.timestamp += secs;
        let ledgers = secs.div_ceil(LEDGER_CLOSE_SECONDS) as u32;
        l.sequence_number = l.sequence_number.saturating_add(ledgers);
    });
}

#[test]
fn dispute_freezes_release_and_claim() {
    let w = world();
    w.escrow
        .raise_dispute(&w.buyer, &w.id, &DisputeReason::NonDelivery, &None);
    assert_eq!(w.escrow.get_escrow(&w.id).status, EscrowStatus::Disputed);

    advance(&w, 3 * DAY);
    assert_eq!(
        w.escrow.try_claim(&w.id),
        Err(Ok(EscrowError::InvalidStatus))
    );
    assert_eq!(
        w.escrow.try_release(&w.id),
        Err(Ok(EscrowError::InvalidStatus))
    );
}

#[test]
fn arbiter_splits_funds() {
    let w = world();
    w.escrow.raise_dispute(
        &w.buyer,
        &w.id,
        &DisputeReason::Defective,
        &Some(hash(&w.env, 1)),
    );
    w.escrow.submit_evidence(&w.seller, &w.id, &hash(&w.env, 2));

    // 30% back to the buyer, 70% to the seller.
    advance(&w, MIN_ESCROW_LOCK_PERIOD);
    w.escrow.resolve(&w.id, &3_000);
    assert_eq!(w.token.balance(&w.buyer), 300);
    assert_eq!(w.token.balance(&w.seller), 700);

    assert_eq!(w.escrow.get_escrow(&w.id).status, EscrowStatus::Resolved);
    let d = w.escrow.get_dispute(&w.id);
    assert_eq!(d.buyer_bps, 3_000);
    assert!(!d.by_default);
}

#[test]
fn arbiter_cannot_rule_after_deadline() {
    let w = world();
    w.escrow
        .raise_dispute(&w.buyer, &w.id, &DisputeReason::WrongItem, &None);
    advance(&w, 2 * DAY + 1);
    assert_eq!(
        w.escrow.try_resolve(&w.id, &5_000),
        Err(Ok(EscrowError::DeadlinePassed))
    );
}

#[test]
fn absent_arbiter_triggers_full_refund_for_non_delivery() {
    let w = world();
    w.escrow
        .raise_dispute(&w.buyer, &w.id, &DisputeReason::NonDelivery, &None);

    assert_eq!(
        w.escrow.try_resolve_by_default(&w.id),
        Err(Ok(EscrowError::DeadlineNotPassed))
    );
    advance(&w, 2 * DAY + 1);
    assert_eq!(w.escrow.resolve_by_default(&w.id), 10_000);
    assert_eq!(w.token.balance(&w.buyer), 1_000);
    assert!(w.escrow.get_dispute(&w.id).by_default);
}

#[test]
fn absent_arbiter_splits_late_delivery() {
    let w = world();
    w.escrow
        .raise_dispute(&w.buyer, &w.id, &DisputeReason::LateDelivery, &None);
    advance(&w, 2 * DAY + 1);
    w.escrow.resolve_by_default(&w.id);
    assert_eq!(w.token.balance(&w.buyer), 500);
    assert_eq!(w.token.balance(&w.seller), 500);
}

#[test]
fn seller_wins_by_default_against_unresponsive_buyer() {
    let w = world();
    w.escrow.raise_dispute(
        &w.seller,
        &w.id,
        &DisputeReason::BuyerUnresponsive,
        &Some(hash(&w.env, 9)),
    );
    advance(&w, 2 * DAY + 1);
    assert_eq!(w.escrow.resolve_by_default(&w.id), 0);
    assert_eq!(w.token.balance(&w.seller), 1_000);
}

#[test]
fn dispute_after_partial_release_only_splits_the_remainder() {
    let w = world();
    w.escrow.release_partial(&w.id, &400);
    w.escrow
        .raise_dispute(&w.buyer, &w.id, &DisputeReason::NotAsDescribed, &None);
    w.escrow.resolve(&w.id, &10_000);
    assert_eq!(w.token.balance(&w.seller), 400);
    assert_eq!(w.token.balance(&w.buyer), 600);
}

#[test]
fn reasons_are_role_restricted() {
    let w = world();
    assert_eq!(
        w.escrow
            .try_raise_dispute(&w.seller, &w.id, &DisputeReason::NonDelivery, &None),
        Err(Ok(EscrowError::ReasonNotAllowed))
    );
    assert_eq!(
        w.escrow
            .try_raise_dispute(&w.buyer, &w.id, &DisputeReason::BuyerUnresponsive, &None),
        Err(Ok(EscrowError::ReasonNotAllowed))
    );
    let outsider = Address::generate(&w.env);
    assert_eq!(
        w.escrow
            .try_raise_dispute(&outsider, &w.id, &DisputeReason::Other, &None),
        Err(Ok(EscrowError::NotParty))
    );
}

#[test]
fn post_release_grace_period_prevents_claim_front_running() {
    let w = world();
    advance(&w, 3 * DAY);
    assert_eq!(w.escrow.try_claim(&w.id), Err(Ok(EscrowError::ReleaseNotDue)));
    w.escrow
        .raise_dispute(&w.buyer, &w.id, &DisputeReason::NonDelivery, &None);
}

#[test]
fn claim_opens_only_after_the_settlement_grace_period() {
    let w = world();
    advance(&w, 3 * DAY + SETTLEMENT_GRACE_PERIOD + 1);
    assert_eq!(
        w.escrow
            .try_raise_dispute(&w.buyer, &w.id, &DisputeReason::NonDelivery, &None),
        Err(Ok(EscrowError::DisputeWindowClosed))
    );
    w.escrow.claim(&w.id);
}

#[test]
fn evidence_is_capped_and_party_only() {
    let w = world();
    assert_eq!(
        w.escrow
            .try_submit_evidence(&w.buyer, &w.id, &hash(&w.env, 0)),
        Err(Ok(EscrowError::InvalidStatus))
    );
    w.escrow
        .raise_dispute(&w.buyer, &w.id, &DisputeReason::Other, &None);
    for i in 0..MAX_EVIDENCE {
        w.escrow
            .submit_evidence(&w.buyer, &w.id, &hash(&w.env, i as u8));
    }
    assert_eq!(
        w.escrow
            .try_submit_evidence(&w.seller, &w.id, &hash(&w.env, 99)),
        Err(Ok(EscrowError::TooMuchEvidence))
    );
    assert_eq!(
        w.escrow
            .try_submit_evidence(&w.arbiter, &w.id, &hash(&w.env, 1)),
        Err(Ok(EscrowError::NotParty))
    );
    assert_eq!(w.escrow.get_evidence(&w.id).len(), MAX_EVIDENCE);
}

#[test]
fn split_above_one_hundred_percent_is_rejected() {
    let w = world();
    w.escrow
        .raise_dispute(&w.buyer, &w.id, &DisputeReason::Other, &None);
    assert_eq!(
        w.escrow.try_resolve(&w.id, &10_001),
        Err(Ok(EscrowError::InvalidSplit))
    );
}

#[test]
fn dispute_deadline_uses_ledger_sequence_not_wall_clock_only() {
    let w = world();
    w.escrow
        .raise_dispute(&w.buyer, &w.id, &DisputeReason::NonDelivery, &None);

    // A large wall-clock jump alone must not expire the dispute while the
    // ledger sequence is unchanged.
    w.env.ledger().with_mut(|l| l.timestamp += 10 * DAY);
    w.escrow.resolve(&w.id, &5_000);
}

#[test]
fn initialized_contract_reports_current_schema_and_migrate_is_idempotent() {
    let w = world();
    assert_eq!(w.escrow.schema_version(), CURRENT_SCHEMA_VERSION);
    assert_eq!(w.escrow.migrate(), CURRENT_SCHEMA_VERSION);
    assert_eq!(w.escrow.schema_version(), CURRENT_SCHEMA_VERSION);
}
