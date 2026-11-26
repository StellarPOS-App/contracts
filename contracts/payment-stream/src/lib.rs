//! Payment streams.
//!
//! A payer locks `deposit` of a token that unlocks to a recipient linearly,
//! second by second, between `start` and `end`: rent, a subscription, or a
//! staff member's pay. The recipient withdraws whatever has accrued whenever
//! they like. Either party can cancel: the recipient keeps everything earned
//! so far, and the payer gets the unearned rest back, in the same
//! transaction.
//!
//! ```text
//! streamed(t) = deposit * (min(t, end) - start) / (end - start)   for t >= start
//! available   = streamed(t) - withdrawn
//! ```
//!
//! Integer division rounds toward the payer until `end`, when the full
//! deposit becomes available, so no dust is ever stuck in the contract.

#![no_std]

use shared::{extend_instance, extend_persistent};
use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, token, Address, Env,
};

/// Shortest allowed stream: one minute.
pub const MIN_DURATION: u64 = 60;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum StreamError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Paused = 3,
    InvalidAmount = 4,
    InvalidSchedule = 5,
    InvalidParties = 6,
    StreamNotFound = 7,
    NotParty = 8,
    NothingToWithdraw = 9,
    StreamEnded = 10,
    Overflow = 11,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum StreamStatus {
    Active,
    /// Fully withdrawn after `end`.
    Settled,
    Cancelled,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Stream {
    pub id: u64,
    pub payer: Address,
    pub recipient: Address,
    pub asset: Address,
    pub deposit: i128,
    pub withdrawn: i128,
    pub start: u64,
    pub end: u64,
    pub status: StreamStatus,
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Admin,
    Paused,
    Count,
    Stream(u64),
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamCreated {
    #[topic]
    pub id: u64,
    #[topic]
    pub recipient: Address,
    pub payer: Address,
    pub deposit: i128,
    pub start: u64,
    pub end: u64,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamWithdrawn {
    #[topic]
    pub id: u64,
    pub amount: i128,
    pub total_withdrawn: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamCancelled {
    #[topic]
    pub id: u64,
    pub by: Address,
    pub to_recipient: i128,
    pub to_payer: i128,
}

// Build metadata embedded in the wasm (SEP-0055); see scripts/verify.sh.
soroban_sdk::contractmeta!(key = "source_repo", val = "github:StellarPOS-App/contracts");
soroban_sdk::contractmeta!(key = "home_domain", val = "github.com/StellarPOS-App");
soroban_sdk::contractmeta!(key = "desc", val = "StellarPOS payment streams");

#[contract]
pub struct PaymentStream;

#[contractimpl]
impl PaymentStream {
    pub fn initialize(env: Env, admin: Address) -> Result<(), StreamError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(StreamError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Paused, &false);
        env.storage().instance().set(&DataKey::Count, &0u64);
        extend_instance(&env);
        Ok(())
    }

    /// Opens a stream and pulls the full deposit from the payer.
    pub fn create(
        env: Env,
        payer: Address,
        recipient: Address,
        asset: Address,
        deposit: i128,
        start: u64,
        end: u64,
    ) -> Result<u64, StreamError> {
        Self::require_not_paused(&env)?;
        payer.require_auth();
        if deposit <= 0 {
            return Err(StreamError::InvalidAmount);
        }
        if payer == recipient {
            return Err(StreamError::InvalidParties);
        }
        if start < env.ledger().timestamp() || end < start.saturating_add(MIN_DURATION) {
            return Err(StreamError::InvalidSchedule);
        }

        token::Client::new(&env, &asset).transfer(&payer, env.current_contract_address(), &deposit);

        let id: u64 = env.storage().instance().get(&DataKey::Count).unwrap_or(0) + 1;
        let stream = Stream {
            id,
            payer: payer.clone(),
            recipient: recipient.clone(),
            asset,
            deposit,
            withdrawn: 0,
            start,
            end,
            status: StreamStatus::Active,
        };
        Self::save(&env, &stream);
        env.storage().instance().set(&DataKey::Count, &id);
        extend_instance(&env);

        StreamCreated {
            id,
            recipient,
            payer,
            deposit,
            start,
            end,
        }
        .publish(&env);
        Ok(id)
    }

    /// Recipient withdraws everything accrued so far. Returns the amount.
    pub fn withdraw(env: Env, id: u64) -> Result<i128, StreamError> {
        Self::require_not_paused(&env)?;
        let mut stream = Self::active(&env, id)?;
        stream.recipient.require_auth();

        let amount = Self::available_now(&env, &stream)?;
        if amount <= 0 {
            return Err(StreamError::NothingToWithdraw);
        }
        token::Client::new(&env, &stream.asset).transfer(
            &env.current_contract_address(),
            &stream.recipient,
            &amount,
        );
        stream.withdrawn += amount;
        if stream.withdrawn == stream.deposit {
            stream.status = StreamStatus::Settled;
        }
        Self::save(&env, &stream);

        StreamWithdrawn {
            id,
            amount,
            total_withdrawn: stream.withdrawn,
        }
        .publish(&env);
        Ok(amount)
    }

    /// Payer or recipient ends the stream early. The recipient receives what
    /// has accrued and not been withdrawn; the payer gets the rest. Works
    /// while paused, because it only returns funds to their owners.
    pub fn cancel(env: Env, caller: Address, id: u64) -> Result<(i128, i128), StreamError> {
        caller.require_auth();
        let mut stream = Self::active(&env, id)?;
        if caller != stream.payer && caller != stream.recipient {
            return Err(StreamError::NotParty);
        }
        if env.ledger().timestamp() >= stream.end {
            return Err(StreamError::StreamEnded);
        }

        let to_recipient = Self::available_now(&env, &stream)?;
        let to_payer = stream.deposit - stream.withdrawn - to_recipient;
        let token = token::Client::new(&env, &stream.asset);
        let this = env.current_contract_address();
        if to_recipient > 0 {
            token.transfer(&this, &stream.recipient, &to_recipient);
        }
        if to_payer > 0 {
            token.transfer(&this, &stream.payer, &to_payer);
        }
        stream.withdrawn += to_recipient;
        stream.status = StreamStatus::Cancelled;
        Self::save(&env, &stream);

        StreamCancelled {
            id,
            by: caller,
            to_recipient,
            to_payer,
        }
        .publish(&env);
        Ok((to_recipient, to_payer))
    }

    // ---------------------------------------------------------------------
    // Admin and views
    // ---------------------------------------------------------------------

    pub fn pause(env: Env) -> Result<(), StreamError> {
        Self::admin(&env)?.require_auth();
        env.storage().instance().set(&DataKey::Paused, &true);
        Ok(())
    }

    pub fn unpause(env: Env) -> Result<(), StreamError> {
        Self::admin(&env)?.require_auth();
        env.storage().instance().set(&DataKey::Paused, &false);
        Ok(())
    }

    pub fn get_stream(env: Env, id: u64) -> Result<Stream, StreamError> {
        Self::load(&env, id)
    }

    /// How much the recipient could withdraw right now.
    pub fn available(env: Env, id: u64) -> Result<i128, StreamError> {
        let stream = Self::load(&env, id)?;
        if stream.status != StreamStatus::Active {
            return Ok(0);
        }
        Self::available_now(&env, &stream)
    }

    pub fn stream_count(env: Env) -> u64 {
        env.storage().instance().get(&DataKey::Count).unwrap_or(0)
    }

    // ---------------------------------------------------------------------
    // Internals
    // ---------------------------------------------------------------------

    /// Amount unlocked at `now` (before subtracting withdrawals).
    pub(crate) fn streamed(stream: &Stream, now: u64) -> Result<i128, StreamError> {
        if now <= stream.start {
            return Ok(0);
        }
        if now >= stream.end {
            return Ok(stream.deposit);
        }
        let elapsed = (now - stream.start) as i128;
        let duration = (stream.end - stream.start) as i128;
        Ok(stream
            .deposit
            .checked_mul(elapsed)
            .ok_or(StreamError::Overflow)?
            / duration)
    }

    fn available_now(env: &Env, stream: &Stream) -> Result<i128, StreamError> {
        Ok(Self::streamed(stream, env.ledger().timestamp())? - stream.withdrawn)
    }

    fn admin(env: &Env) -> Result<Address, StreamError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(StreamError::NotInitialized)
    }

    fn require_not_paused(env: &Env) -> Result<(), StreamError> {
        if env
            .storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
        {
            return Err(StreamError::Paused);
        }
        Ok(())
    }

    fn active(env: &Env, id: u64) -> Result<Stream, StreamError> {
        let stream = Self::load(env, id)?;
        if stream.status != StreamStatus::Active {
            return Err(StreamError::StreamEnded);
        }
        Ok(stream)
    }

    fn load(env: &Env, id: u64) -> Result<Stream, StreamError> {
        let key = DataKey::Stream(id);
        let stream = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(StreamError::StreamNotFound)?;
        extend_persistent(env, &key);
        Ok(stream)
    }

    fn save(env: &Env, stream: &Stream) {
        let key = DataKey::Stream(stream.id);
        env.storage().persistent().set(&key, stream);
        extend_persistent(env, &key);
    }
}

#[cfg(test)]
mod test;
