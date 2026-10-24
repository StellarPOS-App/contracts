//! Treasury yield-vault actions, against an honest mock vault and a greedy one.
extern crate std;

use super::*;
use soroban_sdk::{
    contract, contractimpl, contracttype,
    testutils::Address as _,
    token::{self, StellarAssetClient},
    vec, Address, Env, String,
};

#[contracttype]
enum VKey {
    Token,
    Total,
    Shares(Address),
}

/// ERC-4626-style vault: shares track a proportional claim on its balance.
#[contract]
pub struct MockVault;

#[contractimpl]
impl MockVault {
    pub fn init(env: Env, token: Address) {
        env.storage().instance().set(&VKey::Token, &token);
    }

    pub fn deposit(env: Env, from: Address, amount: i128) -> i128 {
        from.require_auth();
        let token: Address = env.storage().instance().get(&VKey::Token).unwrap();
        let tc = token::Client::new(&env, &token);
        let assets = tc.balance(&env.current_contract_address());
        let total: i128 = env.storage().instance().get(&VKey::Total).unwrap_or(0);
        let minted = if total == 0 {
            amount
        } else {
            amount * total / assets
        };
        tc.transfer_from(
            &env.current_contract_address(),
            &from,
            &env.current_contract_address(),
            &amount,
        );
        let held: i128 = env
            .storage()
            .instance()
            .get(&VKey::Shares(from.clone()))
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&VKey::Shares(from), &(held + minted));
        env.storage()
            .instance()
            .set(&VKey::Total, &(total + minted));
        minted
    }

    pub fn withdraw(env: Env, owner: Address, shares: i128) -> i128 {
        owner.require_auth();
        let token: Address = env.storage().instance().get(&VKey::Token).unwrap();
        let tc = token::Client::new(&env, &token);
        let total: i128 = env.storage().instance().get(&VKey::Total).unwrap();
        let out = shares * tc.balance(&env.current_contract_address()) / total;
        let held: i128 = env
            .storage()
            .instance()
            .get(&VKey::Shares(owner.clone()))
            .unwrap_or(0);
        assert!(held >= shares, "not enough shares");
        env.storage()
            .instance()
            .set(&VKey::Shares(owner.clone()), &(held - shares));
        env.storage()
            .instance()
            .set(&VKey::Total, &(total - shares));
        tc.transfer(&env.current_contract_address(), &owner, &out);
        out
    }

    pub fn shares(env: Env, owner: Address) -> i128 {
        env.storage()
            .instance()
            .get(&VKey::Shares(owner))
            .unwrap_or(0)
    }
}

/// A vault that tries to take twice what it was allowed.
#[contract]
pub struct GreedyVault;

#[contractimpl]
impl GreedyVault {
    pub fn init(env: Env, token: Address) {
        env.storage().instance().set(&VKey::Token, &token);
    }
    pub fn deposit(env: Env, from: Address, amount: i128) -> i128 {
        let token: Address = env.storage().instance().get(&VKey::Token).unwrap();
        token::Client::new(&env, &token).transfer_from(
            &env.current_contract_address(),
            &from,
            &env.current_contract_address(),
            &(amount * 2),
        );
        amount
    }
    pub fn withdraw(_env: Env, _owner: Address, shares: i128) -> i128 {
        shares
    }
    pub fn shares(_env: Env, _owner: Address) -> i128 {
        0
    }
}

struct World<'a> {
    env: Env,
    t: TreasuryClient<'a>,
    a: Address,
    b: Address,
    asset: Address,
    vault: Address,
}

/// 2-of-2 treasury holding 1,000 units, plus an (unapproved) honest vault.
fn world() -> World<'static> {
    let env = Env::default();
    env.mock_all_auths();
    let (a, b) = (Address::generate(&env), Address::generate(&env));
    let t = TreasuryClient::new(&env, &env.register(Treasury, ()));
    t.initialize(&vec![&env, a.clone(), b.clone()], &2);
    let asset = env
        .register_stellar_asset_contract_v2(Address::generate(&env))
        .address();
    StellarAssetClient::new(&env, &asset).mint(&t.address, &1_000);
    let vault = env.register(MockVault, ());
    MockVaultClient::new(&env, &vault).init(&asset);
    World {
        env,
        t,
        a,
        b,
        asset,
        vault,
    }
}

fn pass(w: &World, action: Action) -> Result<(), Result<TreasuryError, soroban_sdk::InvokeError>> {
    let id =
        w.t.propose(&w.a, &action, &String::from_str(&w.env, "yield"));
    w.t.approve(&w.b, &id);
    w.t.try_execute(&id).map(|_| ())
}

fn balance(w: &World, who: &Address) -> i128 {
    token::Client::new(&w.env, &w.asset).balance(who)
}

#[test]
fn deposit_earn_and_withdraw_through_multisig() {
    let w = world();
    assert_eq!(
        pass(
            &w,
            Action::VaultDeposit(w.vault.clone(), w.asset.clone(), 400)
        ),
        Err(Ok(TreasuryError::VaultNotApproved))
    );
    pass(&w, Action::ApproveVault(w.vault.clone())).unwrap();
    assert_eq!(w.t.vaults(), vec![&w.env, w.vault.clone()]);

    pass(
        &w,
        Action::VaultDeposit(w.vault.clone(), w.asset.clone(), 400),
    )
    .unwrap();
    assert_eq!(w.t.balance(&w.asset), 600);
    assert_eq!(balance(&w, &w.vault), 400);
    assert_eq!(w.t.vault_shares(&w.vault), 400);

    // The vault earns 10%.
    StellarAssetClient::new(&w.env, &w.asset).mint(&w.vault, &40);
    pass(&w, Action::VaultWithdraw(w.vault.clone(), 400)).unwrap();
    assert_eq!(w.t.balance(&w.asset), 1_040);
    assert_eq!(w.t.vault_shares(&w.vault), 0);
}

#[test]
fn a_vault_cannot_take_more_than_approved() {
    let w = world();
    let greedy = w.env.register(GreedyVault, ());
    GreedyVaultClient::new(&w.env, &greedy).init(&w.asset);
    pass(&w, Action::ApproveVault(greedy.clone())).unwrap();
    assert!(pass(
        &w,
        Action::VaultDeposit(greedy.clone(), w.asset.clone(), 100)
    )
    .is_err());
    // Nothing moved: the whole proposal reverted.
    assert_eq!(w.t.balance(&w.asset), 1_000);
    assert_eq!(balance(&w, &greedy), 0);
}

#[test]
fn freeze_blocks_deposits_but_not_withdrawals() {
    let w = world();
    pass(&w, Action::ApproveVault(w.vault.clone())).unwrap();
    pass(
        &w,
        Action::VaultDeposit(w.vault.clone(), w.asset.clone(), 300),
    )
    .unwrap();
    w.t.freeze(&w.a);
    assert_eq!(
        pass(
            &w,
            Action::VaultDeposit(w.vault.clone(), w.asset.clone(), 100)
        ),
        Err(Ok(TreasuryError::Frozen))
    );
    pass(&w, Action::VaultWithdraw(w.vault.clone(), 300)).unwrap();
    assert_eq!(w.t.balance(&w.asset), 1_000);
}

#[test]
fn revoking_stops_deposits_but_funds_can_still_come_home() {
    let w = world();
    pass(&w, Action::ApproveVault(w.vault.clone())).unwrap();
    pass(
        &w,
        Action::VaultDeposit(w.vault.clone(), w.asset.clone(), 250),
    )
    .unwrap();
    pass(&w, Action::RevokeVault(w.vault.clone())).unwrap();
    assert_eq!(
        pass(
            &w,
            Action::VaultDeposit(w.vault.clone(), w.asset.clone(), 1)
        ),
        Err(Ok(TreasuryError::VaultNotApproved))
    );
    pass(&w, Action::VaultWithdraw(w.vault.clone(), 250)).unwrap();
    assert_eq!(w.t.balance(&w.asset), 1_000);
    assert_eq!(
        pass(&w, Action::RevokeVault(w.vault.clone())),
        Err(Ok(TreasuryError::VaultNotApproved))
    );
}

#[test]
fn vault_limits_and_validation() {
    let w = world();
    pass(&w, Action::ApproveVault(w.vault.clone())).unwrap();
    assert_eq!(
        pass(
            &w,
            Action::VaultDeposit(w.vault.clone(), w.asset.clone(), 5_000)
        ),
        Err(Ok(TreasuryError::InsufficientBalance))
    );
    assert_eq!(
        w.t.try_propose(
            &w.a,
            &Action::VaultDeposit(w.vault.clone(), w.asset.clone(), 0),
            &String::from_str(&w.env, "zero")
        ),
        Err(Ok(TreasuryError::InvalidAmount))
    );
    for _ in 1..MAX_VAULTS {
        pass(&w, Action::ApproveVault(Address::generate(&w.env))).unwrap();
    }
    assert_eq!(
        pass(&w, Action::ApproveVault(Address::generate(&w.env))),
        Err(Ok(TreasuryError::TooManyVaults))
    );
}
