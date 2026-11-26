extern crate std;

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::StellarAssetClient,
    Env,
};

const T0: u64 = 1_000_000;
const HOUR: u64 = 3_600;

struct Setup<'a> {
    env: Env,
    c: PaymentStreamClient<'a>,
    token: token::Client<'a>,
    asset: Address,
    payer: Address,
    recipient: Address,
}

fn setup() -> Setup<'static> {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().with_mut(|l| l.timestamp = T0);
    let c = PaymentStreamClient::new(&env, &env.register(PaymentStream, ()));
    c.initialize(&Address::generate(&env));
    let asset = env
        .register_stellar_asset_contract_v2(Address::generate(&env))
        .address();
    let payer = Address::generate(&env);
    StellarAssetClient::new(&env, &asset).mint(&payer, &100_000);
    Setup {
        token: token::Client::new(&env, &asset),
        c,
        asset,
        payer,
        recipient: Address::generate(&env),
        env,
    }
}

/// 3,600 units over one hour starting now: one unit per second.
fn open(s: &Setup) -> u64 {
    s.c.create(&s.payer, &s.recipient, &s.asset, &3_600, &T0, &(T0 + HOUR))
}

fn at(s: &Setup, t: u64) {
    s.env.ledger().with_mut(|l| l.timestamp = t);
}

#[test]
fn deposit_is_locked_and_accrues_linearly() {
    let s = setup();
    let id = open(&s);
    assert_eq!(s.token.balance(&s.c.address), 3_600);
    assert_eq!(s.c.available(&id), 0);
    at(&s, T0 + 900);
    assert_eq!(s.c.available(&id), 900);
    at(&s, T0 + HOUR + 10);
    assert_eq!(s.c.available(&id), 3_600);
}

#[test]
fn recipient_withdraws_what_has_accrued() {
    let s = setup();
    let id = open(&s);
    at(&s, T0 + 600);
    assert_eq!(s.c.withdraw(&id), 600);
    assert_eq!(s.token.balance(&s.recipient), 600);
    assert_eq!(
        s.c.try_withdraw(&id),
        Err(Ok(StreamError::NothingToWithdraw))
    );
    at(&s, T0 + 1_000);
    assert_eq!(s.c.withdraw(&id), 400);
    assert_eq!(s.c.get_stream(&id).withdrawn, 1_000);
}

#[test]
fn final_withdrawal_settles_without_dust() {
    let s = setup();
    // 1,000 over 3,600 s does not divide evenly; nothing may be left behind.
    let id =
        s.c.create(&s.payer, &s.recipient, &s.asset, &1_000, &T0, &(T0 + HOUR));
    at(&s, T0 + 1_234);
    let first = s.c.withdraw(&id);
    assert_eq!(first, 342); // floor(1000 * 1234 / 3600)
    at(&s, T0 + HOUR);
    assert_eq!(s.c.withdraw(&id), 1_000 - first);
    assert_eq!(s.token.balance(&s.c.address), 0);
    assert_eq!(s.c.get_stream(&id).status, StreamStatus::Settled);
}

#[test]
fn cancel_splits_earned_and_unearned() {
    let s = setup();
    let id = open(&s);
    at(&s, T0 + 1_000);
    s.c.withdraw(&id); // 1,000 already out
    at(&s, T0 + 1_500);
    assert_eq!(s.c.cancel(&s.payer, &id), (500, 2_100));
    assert_eq!(s.token.balance(&s.recipient), 1_500);
    assert_eq!(s.token.balance(&s.payer), 100_000 - 1_500);
    assert_eq!(s.token.balance(&s.c.address), 0);
    assert_eq!(s.c.get_stream(&id).status, StreamStatus::Cancelled);
    assert_eq!(s.c.available(&id), 0);
}

#[test]
fn only_parties_can_cancel_and_not_after_the_end() {
    let s = setup();
    let id = open(&s);
    assert_eq!(
        s.c.try_cancel(&Address::generate(&s.env), &id),
        Err(Ok(StreamError::NotParty))
    );
    s.c.cancel(&s.recipient, &id);
    assert_eq!(
        s.c.try_cancel(&s.payer, &id),
        Err(Ok(StreamError::StreamEnded))
    );

    let late = open(&s);
    at(&s, T0 + HOUR);
    assert_eq!(
        s.c.try_cancel(&s.payer, &late),
        Err(Ok(StreamError::StreamEnded))
    );
}

#[test]
fn future_start_accrues_nothing_until_it_begins() {
    let s = setup();
    let id = s.c.create(
        &s.payer,
        &s.recipient,
        &s.asset,
        &600,
        &(T0 + 100),
        &(T0 + 700),
    );
    at(&s, T0 + 50);
    assert_eq!(s.c.available(&id), 0);
    // Cancelling before the start refunds everything.
    assert_eq!(s.c.cancel(&s.payer, &id), (0, 600));
}

#[test]
fn creation_validation() {
    let s = setup();
    let c = &s.c;
    assert_eq!(
        c.try_create(&s.payer, &s.recipient, &s.asset, &0, &T0, &(T0 + HOUR)),
        Err(Ok(StreamError::InvalidAmount))
    );
    assert_eq!(
        c.try_create(&s.payer, &s.payer, &s.asset, &10, &T0, &(T0 + HOUR)),
        Err(Ok(StreamError::InvalidParties))
    );
    assert_eq!(
        c.try_create(
            &s.payer,
            &s.recipient,
            &s.asset,
            &10,
            &(T0 - 1),
            &(T0 + HOUR)
        ),
        Err(Ok(StreamError::InvalidSchedule))
    );
    assert_eq!(
        c.try_create(&s.payer, &s.recipient, &s.asset, &10, &T0, &(T0 + 59)),
        Err(Ok(StreamError::InvalidSchedule))
    );
}

#[test]
fn pause_blocks_new_streams_and_withdrawals_but_not_cancel() {
    let s = setup();
    let id = open(&s);
    at(&s, T0 + 60);
    s.c.pause();
    assert_eq!(s.c.try_withdraw(&id), Err(Ok(StreamError::Paused)));
    assert_eq!(
        s.c.try_create(
            &s.payer,
            &s.recipient,
            &s.asset,
            &10,
            &(T0 + 60),
            &(T0 + 600)
        ),
        Err(Ok(StreamError::Paused))
    );
    assert_eq!(s.c.cancel(&s.recipient, &id), (60, 3_540));
}

#[test]
fn streamed_never_overflows_silently() {
    let stream = Stream {
        id: 1,
        payer: Address::generate(&Env::default()),
        recipient: Address::generate(&Env::default()),
        asset: Address::generate(&Env::default()),
        deposit: i128::MAX,
        withdrawn: 0,
        start: 0,
        end: 100,
        status: StreamStatus::Active,
    };
    assert_eq!(
        PaymentStream::streamed(&stream, 50),
        Err(StreamError::Overflow)
    );
    assert_eq!(PaymentStream::streamed(&stream, 100), Ok(i128::MAX));
}
