//! Multi-signature treasury.
//!
//! Receives protocol fees from the payment processor. Nothing leaves the
//! treasury, and none of its rules change, without `threshold` distinct
//! signers approving a proposal:
//!
//! * `Withdraw` funds of any token to any address.
//! * `AddSigner` / `RemoveSigner` / `SetThreshold` to rotate keys.
//! * `Unfreeze` to lift an emergency freeze.
//!
//! Any single signer can `freeze` the treasury immediately (an emergency
//! brake for a suspected key compromise); thawing needs the full threshold.
//! Proposals expire after `PROPOSAL_TTL_SECS` so stale approvals cannot be
//! executed months later.

#![no_std]

use shared::{extend_instance, extend_persistent};
use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, token, Address, Env,
    String, Vec,
};

pub const MAX_SIGNERS: u32 = 10;
pub const PROPOSAL_TTL_SECS: u64 = 7 * 24 * 60 * 60;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    NotSigner = 3,
    InvalidThreshold = 4,
    InvalidSigners = 5,
    ProposalNotFound = 6,
    AlreadyApproved = 7,
    AlreadyExecuted = 8,
    ProposalExpired = 9,
    NotEnoughApprovals = 10,
    Frozen = 11,
    InvalidAmount = 12,
    InsufficientBalance = 13,
    SignerExists = 14,
    Cancelled = 15,
    VaultNotApproved = 16,
    TooManyVaults = 17,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    Withdraw(Address, Address, i128),
    AddSigner(Address),
    RemoveSigner(Address),
    SetThreshold(u32),
    Unfreeze,
    /// Allow a yield vault to receive treasury deposits.
    ApproveVault(Address),
    /// Stop new deposits into a vault (withdrawing stays possible).
    RevokeVault(Address),
    /// Deposit (vault, token, amount) into an approved vault.
    VaultDeposit(Address, Address, i128),
    /// Redeem (vault, shares) back into the treasury.
    VaultWithdraw(Address, i128),
}

/// Most vaults approved at once.
pub const MAX_VAULTS: u32 = 5;

/// The interface a yield vault must implement (ERC-4626 style). The
/// treasury only ever calls these three functions.
#[allow(dead_code)]
#[soroban_sdk::contractclient(name = "VaultClient")]
pub trait VaultInterface {
    /// Pulls `amount` of the vault's token from `from` (via allowance) and
    /// returns the shares minted to `from`.
    fn deposit(env: Env, from: Address, amount: i128) -> i128;
    /// Burns `shares` of `owner` and pays the underlying to `owner`.
    fn withdraw(env: Env, owner: Address, shares: i128) -> i128;
    /// Shares held by `owner`.
    fn shares(env: Env, owner: Address) -> i128;
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ProposalStatus {
    Open,
    Executed,
    Cancelled,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Proposal {
    pub id: u64,
    pub proposer: Address,
    pub action: Action,
    pub memo: String,
    pub approvals: Vec<Address>,
    pub status: ProposalStatus,
    pub created_at: u64,
    pub expires_at: u64,
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Signers,
    Threshold,
    Frozen,
    ProposalCount,
    Proposal(u64),
    Vaults,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProposalCreated {
    #[topic]
    pub id: u64,
    pub proposer: Address,
    pub action: Action,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProposalApproved {
    #[topic]
    pub id: u64,
    pub signer: Address,
    pub approvals: u32,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProposalExecuted {
    #[topic]
    pub id: u64,
    pub action: Action,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreasuryFrozen {
    pub by: Address,
    pub frozen: bool,
}

// Build metadata embedded in the wasm (SEP-0055), so anyone can trace a
// deployed contract back to its source; see scripts/verify.sh.
soroban_sdk::contractmeta!(key = "source_repo", val = "github:StellarPOS-App/contracts");
soroban_sdk::contractmeta!(key = "home_domain", val = "github.com/StellarPOS-App");
soroban_sdk::contractmeta!(key = "desc", val = "StellarPOS treasury");

#[contract]
pub struct Treasury;

#[contractimpl]
impl Treasury {
    pub fn initialize(
        env: Env,
        signers: Vec<Address>,
        threshold: u32,
    ) -> Result<(), TreasuryError> {
        if env.storage().instance().has(&DataKey::Signers) {
            return Err(TreasuryError::AlreadyInitialized);
        }
        Self::validate_signers(&signers)?;
        if threshold == 0 || threshold > signers.len() {
            return Err(TreasuryError::InvalidThreshold);
        }
        for signer in signers.iter() {
            signer.require_auth();
        }
        env.storage().instance().set(&DataKey::Signers, &signers);
        env.storage()
            .instance()
            .set(&DataKey::Threshold, &threshold);
        env.storage().instance().set(&DataKey::Frozen, &false);
        env.storage().instance().set(&DataKey::ProposalCount, &0u64);
        extend_instance(&env);
        Ok(())
    }

    /// Opens a proposal. The proposer's approval is counted immediately.
    pub fn propose(
        env: Env,
        proposer: Address,
        action: Action,
        memo: String,
    ) -> Result<u64, TreasuryError> {
        proposer.require_auth();
        Self::require_signer(&env, &proposer)?;
        match &action {
            Action::Withdraw(_, _, amount)
            | Action::VaultDeposit(_, _, amount)
            | Action::VaultWithdraw(_, amount)
                if *amount <= 0 =>
            {
                return Err(TreasuryError::InvalidAmount);
            }
            _ => {}
        }

        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::ProposalCount)
            .unwrap_or(0)
            + 1;
        let now = env.ledger().timestamp();
        let mut approvals = Vec::new(&env);
        approvals.push_back(proposer.clone());
        let proposal = Proposal {
            id,
            proposer: proposer.clone(),
            action: action.clone(),
            memo,
            approvals,
            status: ProposalStatus::Open,
            created_at: now,
            expires_at: now + PROPOSAL_TTL_SECS,
        };
        Self::save(&env, &proposal);
        env.storage().instance().set(&DataKey::ProposalCount, &id);
        extend_instance(&env);

        ProposalCreated {
            id,
            proposer,
            action,
        }
        .publish(&env);
        Ok(id)
    }

    pub fn approve(env: Env, signer: Address, id: u64) -> Result<u32, TreasuryError> {
        signer.require_auth();
        Self::require_signer(&env, &signer)?;
        let mut proposal = Self::open_proposal(&env, id)?;
        if proposal.approvals.contains(&signer) {
            return Err(TreasuryError::AlreadyApproved);
        }
        proposal.approvals.push_back(signer.clone());
        Self::save(&env, &proposal);

        let approvals = proposal.approvals.len();
        ProposalApproved {
            id,
            signer,
            approvals,
        }
        .publish(&env);
        Ok(approvals)
    }

    /// Executes a proposal once it has enough approvals from *current*
    /// signers. Anyone may submit the execution transaction.
    pub fn execute(env: Env, id: u64) -> Result<(), TreasuryError> {
        let mut proposal = Self::open_proposal(&env, id)?;
        let signers = Self::signers(&env)?;
        let valid = proposal
            .approvals
            .iter()
            .filter(|a| signers.contains(a))
            .count() as u32;
        if valid < Self::threshold(env.clone()) {
            return Err(TreasuryError::NotEnoughApprovals);
        }

        let frozen = Self::is_frozen(env.clone());
        match proposal.action.clone() {
            Action::Withdraw(token_id, to, amount) => {
                if frozen {
                    return Err(TreasuryError::Frozen);
                }
                let token = token::Client::new(&env, &token_id);
                if token.balance(&env.current_contract_address()) < amount {
                    return Err(TreasuryError::InsufficientBalance);
                }
                token.transfer(&env.current_contract_address(), &to, &amount);
            }
            Action::AddSigner(signer) => {
                if signers.contains(&signer) {
                    return Err(TreasuryError::SignerExists);
                }
                let mut next = signers.clone();
                next.push_back(signer);
                Self::validate_signers(&next)?;
                env.storage().instance().set(&DataKey::Signers, &next);
            }
            Action::RemoveSigner(signer) => {
                let idx = signers
                    .first_index_of(&signer)
                    .ok_or(TreasuryError::NotSigner)?;
                let mut next = signers.clone();
                next.remove(idx);
                if next.len() < Self::threshold(env.clone()) {
                    return Err(TreasuryError::InvalidThreshold);
                }
                env.storage().instance().set(&DataKey::Signers, &next);
            }
            Action::SetThreshold(t) => {
                if t == 0 || t > signers.len() {
                    return Err(TreasuryError::InvalidThreshold);
                }
                env.storage().instance().set(&DataKey::Threshold, &t);
            }
            Action::Unfreeze => {
                env.storage().instance().set(&DataKey::Frozen, &false);
            }
            Action::ApproveVault(vault) => {
                let mut vaults = Self::vaults(env.clone());
                if !vaults.contains(&vault) {
                    if vaults.len() >= MAX_VAULTS {
                        return Err(TreasuryError::TooManyVaults);
                    }
                    vaults.push_back(vault);
                    env.storage().instance().set(&DataKey::Vaults, &vaults);
                }
            }
            Action::RevokeVault(vault) => {
                let mut vaults = Self::vaults(env.clone());
                let idx = vaults
                    .first_index_of(&vault)
                    .ok_or(TreasuryError::VaultNotApproved)?;
                vaults.remove(idx);
                env.storage().instance().set(&DataKey::Vaults, &vaults);
            }
            Action::VaultDeposit(vault, token_id, amount) => {
                if frozen {
                    return Err(TreasuryError::Frozen);
                }
                if !Self::vaults(env.clone()).contains(&vault) {
                    return Err(TreasuryError::VaultNotApproved);
                }
                let this = env.current_contract_address();
                let token = token::Client::new(&env, &token_id);
                if token.balance(&this) < amount {
                    return Err(TreasuryError::InsufficientBalance);
                }
                // Allow the vault to pull exactly this amount, for this
                // ledger only, then deposit. The vault can never take more.
                token.approve(&this, &vault, &amount, &env.ledger().sequence());
                VaultClient::new(&env, &vault).deposit(&this, &amount);
            }
            Action::VaultWithdraw(vault, shares) => {
                // Allowed while frozen and for revoked vaults: it only ever
                // brings funds back into the treasury.
                VaultClient::new(&env, &vault).withdraw(&env.current_contract_address(), &shares);
            }
        }
        extend_instance(&env);

        proposal.status = ProposalStatus::Executed;
        Self::save(&env, &proposal);
        ProposalExecuted {
            id,
            action: proposal.action,
        }
        .publish(&env);
        Ok(())
    }

    /// Withdraws a proposal. Only its proposer can cancel it.
    pub fn cancel(env: Env, proposer: Address, id: u64) -> Result<(), TreasuryError> {
        proposer.require_auth();
        let mut proposal = Self::open_proposal(&env, id)?;
        if proposal.proposer != proposer {
            return Err(TreasuryError::NotSigner);
        }
        proposal.status = ProposalStatus::Cancelled;
        Self::save(&env, &proposal);
        Ok(())
    }

    /// Emergency brake: any one signer can block all withdrawals at once.
    pub fn freeze(env: Env, signer: Address) -> Result<(), TreasuryError> {
        signer.require_auth();
        Self::require_signer(&env, &signer)?;
        env.storage().instance().set(&DataKey::Frozen, &true);
        extend_instance(&env);
        TreasuryFrozen {
            by: signer,
            frozen: true,
        }
        .publish(&env);
        Ok(())
    }

    // ---------------------------------------------------------------------
    // Views
    // ---------------------------------------------------------------------

    pub fn get_proposal(env: Env, id: u64) -> Result<Proposal, TreasuryError> {
        Self::load(&env, id)
    }

    pub fn get_signers(env: Env) -> Result<Vec<Address>, TreasuryError> {
        Self::signers(&env)
    }

    pub fn threshold(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::Threshold)
            .unwrap_or(0)
    }

    pub fn is_frozen(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Frozen)
            .unwrap_or(false)
    }

    /// Vaults approved for deposits.
    pub fn vaults(env: Env) -> Vec<Address> {
        env.storage()
            .instance()
            .get(&DataKey::Vaults)
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// Treasury's shares in a vault.
    pub fn vault_shares(env: Env, vault: Address) -> i128 {
        VaultClient::new(&env, &vault).shares(&env.current_contract_address())
    }

    pub fn balance(env: Env, token_id: Address) -> i128 {
        token::Client::new(&env, &token_id).balance(&env.current_contract_address())
    }

    // ---------------------------------------------------------------------
    // Internals
    // ---------------------------------------------------------------------

    fn signers(env: &Env) -> Result<Vec<Address>, TreasuryError> {
        env.storage()
            .instance()
            .get(&DataKey::Signers)
            .ok_or(TreasuryError::NotInitialized)
    }

    fn require_signer(env: &Env, who: &Address) -> Result<(), TreasuryError> {
        if !Self::signers(env)?.contains(who) {
            return Err(TreasuryError::NotSigner);
        }
        Ok(())
    }

    fn validate_signers(signers: &Vec<Address>) -> Result<(), TreasuryError> {
        if signers.is_empty() || signers.len() > MAX_SIGNERS {
            return Err(TreasuryError::InvalidSigners);
        }
        for (i, s) in signers.iter().enumerate() {
            if signers.first_index_of(&s) != Some(i as u32) {
                return Err(TreasuryError::InvalidSigners);
            }
        }
        Ok(())
    }

    fn open_proposal(env: &Env, id: u64) -> Result<Proposal, TreasuryError> {
        let proposal = Self::load(env, id)?;
        match proposal.status {
            ProposalStatus::Executed => return Err(TreasuryError::AlreadyExecuted),
            ProposalStatus::Cancelled => return Err(TreasuryError::Cancelled),
            ProposalStatus::Open => {}
        }
        if env.ledger().timestamp() > proposal.expires_at {
            return Err(TreasuryError::ProposalExpired);
        }
        Ok(proposal)
    }

    fn load(env: &Env, id: u64) -> Result<Proposal, TreasuryError> {
        let key = DataKey::Proposal(id);
        let proposal = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(TreasuryError::ProposalNotFound)?;
        extend_persistent(env, &key);
        Ok(proposal)
    }

    fn save(env: &Env, proposal: &Proposal) {
        let key = DataKey::Proposal(proposal.id);
        env.storage().persistent().set(&key, proposal);
        extend_persistent(env, &key);
    }
}

#[cfg(test)]
mod test;
#[cfg(test)]
mod vault_test;
