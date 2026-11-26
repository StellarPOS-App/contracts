extern crate std;

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::StellarAssetClient,
    vec, Env, String,
};

struct Setup<'a> {
    env: Env,
    t: TreasuryClient<'a>,
    a: Address,
    b: Address,
    c: Address,
    asset: Address,
}

/// 2-of-3 treasury holding 1,000 units.
fn setup() -> Setup<'static> {
    let env = Env::default();
    env.mock_all_auths();
    let (a, b, c) = (
        Address::generate(&env),
        Address::generate(&env),
        Address::generate(&env),
    );
    let id = env.register(Treasury, ());
    let t = TreasuryClient::new(&env, &id);
    t.initialize(&vec![&env, a.clone(), b.clone(), c.clone()], &2);

    let asset = env
        .register_stellar_asset_contract_v2(Address::generate(&env))
        .address();
    StellarAssetClient::new(&env, &asset).mint(&id, &1_000);
    Setup {
        env,
        t,
        a,
        b,
        c,
        asset,
    }
}

fn memo(env: &Env) -> String {
    String::from_str(env, "monthly fee sweep")
}

/// Moves the ledger past the timelock on a proposal created at genesis.
fn mature_funds(s: &Setup) {
    s.env.ledger().set_timestamp(FUND_MOVEMENT_TIMELOCK_SECS);
}

#[test]
fn withdrawal_needs_threshold_approvals() {
    let s = setup();
    let to = Address::generate(&s.env);
    let id = s.t.propose(
        &s.a,
        &Action::Withdraw(s.asset.clone(), to.clone(), 400),
        &memo(&s.env),
    );

    assert_eq!(
        s.t.try_execute(&id),
        Err(Ok(TreasuryError::NotEnoughApprovals))
    );
    assert_eq!(s.t.approve(&s.b, &id), 2);
    mature_funds(&s);
    s.t.execute(&id);

    assert_eq!(s.t.balance(&s.asset), 600);
    assert_eq!(token::Client::new(&s.env, &s.asset).balance(&to), 400);
    assert_eq!(
        s.t.try_execute(&id),
        Err(Ok(TreasuryError::AlreadyExecuted))
    );
}

#[test]
fn non_signers_cannot_propose_or_approve() {
    let s = setup();
    let outsider = Address::generate(&s.env);
    assert_eq!(
        s.t.try_propose(&outsider, &Action::Unfreeze, &memo(&s.env)),
        Err(Ok(TreasuryError::NotSigner))
    );
    let id = s.t.propose(&s.a, &Action::Unfreeze, &memo(&s.env));
    assert_eq!(
        s.t.try_approve(&outsider, &id),
        Err(Ok(TreasuryError::NotSigner))
    );
    assert_eq!(
        s.t.try_approve(&s.a, &id),
        Err(Ok(TreasuryError::AlreadyApproved))
    );
}

#[test]
fn any_signer_can_freeze_but_unfreeze_needs_quorum() {
    let s = setup();
    let to = Address::generate(&s.env);
    let withdraw = s.t.propose(
        &s.a,
        &Action::Withdraw(s.asset.clone(), to, 100),
        &memo(&s.env),
    );
    s.t.approve(&s.b, &withdraw);

    s.t.freeze(&s.c);
    assert!(s.t.is_frozen());
    assert_eq!(s.t.try_execute(&withdraw), Err(Ok(TreasuryError::Frozen)));

    let thaw = s.t.propose(&s.a, &Action::Unfreeze, &memo(&s.env));
    s.t.approve(&s.c, &thaw);
    s.t.execute(&thaw);
    assert!(!s.t.is_frozen());
    mature_funds(&s);
    s.t.execute(&withdraw);
    assert_eq!(s.t.balance(&s.asset), 900);
}

#[test]
fn proposals_expire() {
    let s = setup();
    let id = s.t.propose(&s.a, &Action::Unfreeze, &memo(&s.env));
    s.env
        .ledger()
        .with_mut(|l| l.timestamp += PROPOSAL_TTL_SECS + 1);
    assert_eq!(
        s.t.try_approve(&s.b, &id),
        Err(Ok(TreasuryError::ProposalExpired))
    );
}

#[test]
fn signer_rotation_and_removed_signer_approvals_stop_counting() {
    let s = setup();
    let d = Address::generate(&s.env);

    let add =
        s.t.propose(&s.a, &Action::AddSigner(d.clone()), &memo(&s.env));
    s.t.approve(&s.b, &add);
    s.t.execute(&add);
    assert_eq!(s.t.get_signers().len(), 4);

    // c approves a withdrawal, then gets removed before it executes.
    let to = Address::generate(&s.env);
    let w = s.t.propose(
        &s.a,
        &Action::Withdraw(s.asset.clone(), to, 10),
        &memo(&s.env),
    );
    s.t.approve(&s.c, &w);
    let rm =
        s.t.propose(&s.a, &Action::RemoveSigner(s.c.clone()), &memo(&s.env));
    s.t.approve(&d, &rm);
    s.t.execute(&rm);

    assert_eq!(
        s.t.try_execute(&w),
        Err(Ok(TreasuryError::NotEnoughApprovals))
    );
    s.t.approve(&d, &w);
    mature_funds(&s);
    s.t.execute(&w);
}

#[test]
fn removing_a_signer_cancels_their_open_proposals() {
    let s = setup();
    let to = Address::generate(&s.env);
    // c queues a withdrawal that a and b approve, so it would still have
    // enough valid approvals after c is gone.
    let w = s.t.propose(
        &s.c,
        &Action::Withdraw(s.asset.clone(), to, 10),
        &memo(&s.env),
    );
    s.t.approve(&s.a, &w);
    s.t.approve(&s.b, &w);
    let other = s.t.propose(&s.b, &Action::Unfreeze, &memo(&s.env));

    let rm =
        s.t.propose(&s.a, &Action::RemoveSigner(s.c.clone()), &memo(&s.env));
    s.t.approve(&s.b, &rm);
    s.t.execute(&rm);

    assert_eq!(s.t.get_proposal(&w).status, ProposalStatus::Cancelled);
    assert_eq!(s.t.try_execute(&w), Err(Ok(TreasuryError::Cancelled)));
    assert_eq!(s.t.balance(&s.asset), 1_000);
    assert_eq!(s.t.get_proposal(&other).status, ProposalStatus::Open);
}

#[test]
fn a_signer_can_still_propose_their_own_removal() {
    let s = setup();
    let rm =
        s.t.propose(&s.c, &Action::RemoveSigner(s.c.clone()), &memo(&s.env));
    s.t.approve(&s.a, &rm);
    s.t.execute(&rm);
    assert_eq!(s.t.get_proposal(&rm).status, ProposalStatus::Executed);
    assert_eq!(s.t.get_signers().len(), 2);
}

#[test]
fn cannot_remove_below_threshold_or_set_impossible_threshold() {
    let s = setup();
    let t3 = s.t.propose(&s.a, &Action::SetThreshold(3), &memo(&s.env));
    s.t.approve(&s.b, &t3);
    s.t.execute(&t3);
    assert_eq!(s.t.threshold(), 3);

    let rm =
        s.t.propose(&s.a, &Action::RemoveSigner(s.c.clone()), &memo(&s.env));
    s.t.approve(&s.b, &rm);
    s.t.approve(&s.c, &rm);
    assert_eq!(
        s.t.try_execute(&rm),
        Err(Ok(TreasuryError::InvalidThreshold))
    );

    let t4 = s.t.propose(&s.a, &Action::SetThreshold(4), &memo(&s.env));
    s.t.approve(&s.b, &t4);
    s.t.approve(&s.c, &t4);
    assert_eq!(
        s.t.try_execute(&t4),
        Err(Ok(TreasuryError::InvalidThreshold))
    );
}

#[test]
fn cancelled_proposals_cannot_execute() {
    let s = setup();
    let id = s.t.propose(&s.a, &Action::Unfreeze, &memo(&s.env));
    assert_eq!(s.t.try_cancel(&s.b, &id), Err(Ok(TreasuryError::NotSigner)));
    s.t.cancel(&s.a, &id);
    assert_eq!(
        s.t.try_approve(&s.b, &id),
        Err(Ok(TreasuryError::Cancelled))
    );
}

#[test]
fn initialization_validation() {
    let env = Env::default();
    env.mock_all_auths();
    let t = TreasuryClient::new(&env, &env.register(Treasury, ()));
    let a = Address::generate(&env);
    assert_eq!(
        t.try_initialize(&vec![&env, a.clone()], &2),
        Err(Ok(TreasuryError::InvalidThreshold))
    );
    assert_eq!(
        t.try_initialize(&vec![&env, a.clone(), a.clone()], &1),
        Err(Ok(TreasuryError::InvalidSigners))
    );
    assert_eq!(
        t.try_initialize(&vec![&env], &0),
        Err(Ok(TreasuryError::InvalidSigners))
    );
}

#[test]
fn overdrawn_withdrawal_fails_cleanly() {
    let s = setup();
    let id = s.t.propose(
        &s.a,
        &Action::Withdraw(s.asset.clone(), Address::generate(&s.env), 5_000),
        &memo(&s.env),
    );
    s.t.approve(&s.b, &id);
    mature_funds(&s);
    assert_eq!(
        s.t.try_execute(&id),
        Err(Ok(TreasuryError::InsufficientBalance))
    );
}

#[test]
fn fund_movements_wait_for_the_timelock() {
    let s = setup();
    let to = Address::generate(&s.env);
    let id = s.t.propose(
        &s.a,
        &Action::Withdraw(s.asset.clone(), to.clone(), 100),
        &memo(&s.env),
    );
    s.t.approve(&s.b, &id);

    assert_eq!(s.t.executable_at(&id), FUND_MOVEMENT_TIMELOCK_SECS);
    assert_eq!(
        s.t.try_execute(&id),
        Err(Ok(TreasuryError::TimelockNotElapsed))
    );
    assert_eq!(s.t.balance(&s.asset), 1_000);

    // One second early is still too early.
    s.env
        .ledger()
        .set_timestamp(FUND_MOVEMENT_TIMELOCK_SECS - 1);
    assert_eq!(
        s.t.try_execute(&id),
        Err(Ok(TreasuryError::TimelockNotElapsed))
    );

    s.env.ledger().set_timestamp(FUND_MOVEMENT_TIMELOCK_SECS);
    s.t.execute(&id);
    assert_eq!(s.t.balance(&s.asset), 900);
    assert_eq!(token::Client::new(&s.env, &s.asset).balance(&to), 100);
}

#[test]
fn non_fund_actions_execute_without_waiting() {
    let s = setup();
    let id = s.t.propose(&s.a, &Action::Unfreeze, &memo(&s.env));
    assert_eq!(s.t.executable_at(&id), 0);
    s.t.approve(&s.b, &id);
    s.t.execute(&id);
    assert_eq!(s.t.get_proposal(&id).status, ProposalStatus::Executed);
}
