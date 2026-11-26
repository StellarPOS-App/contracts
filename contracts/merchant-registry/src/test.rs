extern crate std;

use super::*;
use soroban_sdk::{testutils::{Address as _, Ledger}, vec, Env, String};

struct Setup<'a> {
    env: Env,
    client: MerchantRegistryClient<'a>,
    owner: Address,
    settlement: Address,
    usdc: Address,
}

fn setup() -> Setup<'static> {
    let env = Env::default();
    env.mock_all_auths();
    let id = env.register(MerchantRegistry, ());
    let client = MerchantRegistryClient::new(&env, &id);
    let admin = Address::generate(&env);
    client.initialize(&admin);
    Setup {
        owner: Address::generate(&env),
        settlement: Address::generate(&env),
        usdc: Address::generate(&env),
        client,
        env,
    }
}

fn register(s: &Setup) {
    s.client.register(
        &s.owner,
        &String::from_str(&s.env, "Kofi's Coffee"),
        &s.settlement,
        &vec![&s.env, s.usdc.clone()],
    );
}

#[test]
fn registration_starts_pending_and_cannot_accept_payments() {
    let s = setup();
    register(&s);

    let m = s.client.get_merchant(&s.owner);
    assert_eq!(m.status, MerchantStatus::Pending);
    assert_eq!(s.client.merchant_count(), 1);
    assert!(!s.client.is_active(&s.owner));
    assert!(!s.client.accepts(&s.owner, &s.usdc));
}

#[test]
fn approved_merchant_accepts_only_listed_assets() {
    let s = setup();
    register(&s);
    s.client.approve(&s.owner);

    assert!(s.client.is_active(&s.owner));
    assert!(s.client.accepts(&s.owner, &s.usdc));
    assert!(!s.client.accepts(&s.owner, &Address::generate(&s.env)));
    assert_eq!(s.client.settlement_of(&s.owner), s.settlement);
}

#[test]
fn suspension_blocks_settlement_until_reinstated() {
    let s = setup();
    register(&s);
    s.client.approve(&s.owner);
    s.client.suspend(&s.owner);

    assert!(!s.client.accepts(&s.owner, &s.usdc));
    assert_eq!(
        s.client.try_settlement_of(&s.owner),
        Err(Ok(RegistryError::InvalidStatusTransition))
    );

    s.client.reinstate(&s.owner);
    assert!(s.client.is_active(&s.owner));
}

#[test]
fn status_transitions_are_enforced() {
    let s = setup();
    register(&s);
    // Pending merchants cannot be suspended or reinstated.
    assert_eq!(
        s.client.try_suspend(&s.owner),
        Err(Ok(RegistryError::InvalidStatusTransition))
    );
    assert_eq!(
        s.client.try_reinstate(&s.owner),
        Err(Ok(RegistryError::InvalidStatusTransition))
    );
}

#[test]
fn duplicate_registration_is_rejected() {
    let s = setup();
    register(&s);
    let again = s.client.try_register(
        &s.owner,
        &String::from_str(&s.env, "Again"),
        &s.settlement,
        &vec![&s.env],
    );
    assert_eq!(again, Err(Ok(RegistryError::AlreadyRegistered)));
}

#[test]
fn asset_list_validation() {
    let s = setup();
    let dup = s.client.try_register(
        &s.owner,
        &String::from_str(&s.env, "Dupes"),
        &s.settlement,
        &vec![&s.env, s.usdc.clone(), s.usdc.clone()],
    );
    assert_eq!(dup, Err(Ok(RegistryError::DuplicateAsset)));

    let mut many = vec![&s.env];
    for _ in 0..=MAX_ACCEPTED_ASSETS {
        many.push_back(Address::generate(&s.env));
    }
    let too_many = s.client.try_register(
        &s.owner,
        &String::from_str(&s.env, "Many"),
        &s.settlement,
        &many,
    );
    assert_eq!(too_many, Err(Ok(RegistryError::TooManyAssets)));
}

#[test]
fn empty_name_is_rejected() {
    let s = setup();
    let res = s.client.try_register(
        &s.owner,
        &String::from_str(&s.env, ""),
        &s.settlement,
        &vec![&s.env],
    );
    assert_eq!(res, Err(Ok(RegistryError::InvalidName)));
    assert_eq!(s.client.merchant_count(), 0);
    assert_eq!(
        s.client.try_get_merchant(&s.owner),
        Err(Ok(RegistryError::MerchantNotFound))
    );
}

#[test]
fn profile_update_changes_settlement_and_assets() {
    let s = setup();
    register(&s);
    s.client.approve(&s.owner);

    let new_settlement = Address::generate(&s.env);
    let xlm = Address::generate(&s.env);
    s.client
        .update_profile(&s.owner, &new_settlement, &vec![&s.env, xlm.clone()]);

    assert_eq!(s.client.settlement_of(&s.owner), new_settlement);
    assert!(s.client.accepts(&s.owner, &xlm));
    assert!(!s.client.accepts(&s.owner, &s.usdc));
}

#[test]
fn get_merchant_reads_latest_persistent_profile_after_update() {
    let s = setup();
    register(&s);
    s.client.approve(&s.owner);

    let before = s.client.get_merchant(&s.owner);
    s.env.ledger().with_mut(|li| li.timestamp += 1);
    let new_settlement = Address::generate(&s.env);
    let xlm = Address::generate(&s.env);
    s.client
        .update_profile(&s.owner, &new_settlement, &vec![&s.env, xlm.clone()]);

    let after = s.client.get_merchant(&s.owner);
    assert_ne!(after.updated_at, before.updated_at);
    assert_eq!(after.settlement, new_settlement);
    assert_eq!(after.accepted_assets, vec![&s.env, xlm]);
}

#[test]
fn pause_blocks_registration() {
    let s = setup();
    s.client.pause();
    assert!(s.client.is_paused());
    let res = s.client.try_register(
        &s.owner,
        &String::from_str(&s.env, "Paused"),
        &s.settlement,
        &vec![&s.env],
    );
    assert_eq!(res, Err(Ok(RegistryError::Paused)));
    s.client.unpause();
    register(&s);
}

#[test]
fn initialize_twice_fails() {
    let s = setup();
    assert_eq!(
        s.client.try_initialize(&Address::generate(&s.env)),
        Err(Ok(RegistryError::AlreadyInitialized))
    );
}

#[test]
fn approve_many_approves_pending_and_skips_the_rest() {
    let s = setup();
    let mut pending = vec![&s.env];
    for i in 0..3 {
        let owner = Address::generate(&s.env);
        s.client.register(
            &owner,
            &String::from_str(&s.env, if i == 0 { "A" } else { "B" }),
            &s.settlement,
            &vec![&s.env, s.usdc.clone()],
        );
        pending.push_back(owner);
    }
    register(&s);
    s.client.approve(&s.owner);

    // Already-active and unknown owners are skipped, not fatal.
    let mut batch = pending.clone();
    batch.push_back(s.owner.clone());
    batch.push_back(Address::generate(&s.env));
    assert_eq!(s.client.approve_many(&batch), 3);
    for owner in pending.iter() {
        assert!(s.client.is_active(&owner));
    }
}

#[test]
fn approve_many_is_capped() {
    let s = setup();
    let mut batch = vec![&s.env];
    for _ in 0..=MAX_BATCH {
        batch.push_back(Address::generate(&s.env));
    }
    assert_eq!(
        s.client.try_approve_many(&batch),
        Err(Ok(RegistryError::BatchTooLarge))
    );
}

#[test]
fn reject_removes_a_pending_registration_so_it_can_reapply() {
    let s = setup();
    register(&s);
    s.client.reject(&s.owner);
    assert_eq!(
        s.client.try_get_merchant(&s.owner),
        Err(Ok(RegistryError::MerchantNotFound))
    );
    assert_eq!(s.client.merchant_count(), 0);

    register(&s);
    assert_eq!(s.client.merchant_count(), 1);
}

#[test]
fn active_merchants_cannot_be_rejected() {
    let s = setup();
    register(&s);
    s.client.approve(&s.owner);
    assert_eq!(
        s.client.try_reject(&s.owner),
        Err(Ok(RegistryError::InvalidStatusTransition))
    );
}

#[test]
fn checkout_target_returns_settlement_only_when_payable() {
    let s = setup();
    register(&s);
    // Pending: not payable yet.
    assert_eq!(s.client.checkout_target(&s.owner, &s.usdc), None);

    s.client.approve(&s.owner);
    assert_eq!(
        s.client.checkout_target(&s.owner, &s.usdc),
        Some(s.settlement.clone())
    );
    // Asset not on the merchant's list.
    assert_eq!(
        s.client
            .checkout_target(&s.owner, &Address::generate(&s.env)),
        None
    );
    // Unknown merchant.
    assert_eq!(
        s.client
            .checkout_target(&Address::generate(&s.env), &s.usdc),
        None
    );

    s.client.suspend(&s.owner);
    assert_eq!(s.client.checkout_target(&s.owner, &s.usdc), None);
}
