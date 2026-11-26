# Technical Specification: Issues #63, #64, #67

This document provides detailed implementation specifications for the remaining StellarPOS contract issues.

## Issue #64: Two-Step Admin Transfer (PRIORITY: CRITICAL)

### Current State
The payment-processor contract currently has NO admin transfer mechanism. Once initialized, the admin cannot be changed, which creates a permanent lock-in risk.

### Security Risk
- Admin key compromise: No way to rotate admin
- Typo in initialization: Permanent loss of admin control
- No recovery mechanism if admin key is lost

### Proposed Solution: Two-Step Transfer Pattern

#### 1. Add New DataKey
```rust
#[contracttype]
enum DataKey {
    // ... existing keys ...
    PendingAdmin,  // Address waiting to accept admin role
}
```

#### 2. Add New Error Codes
```rust
pub enum PaymentError {
    // ... existing errors ...
    NoPendingTransfer = 26,     // No pending admin transfer exists
    NotPendingAdmin = 27,       // Caller is not the pending admin
    TransferAlreadyPending = 28, // A transfer is already in progress
}
```

#### 3. Add Admin Transfer Functions

```rust
/// Step 1: Current admin proposes a new admin
pub fn transfer_admin(env: Env, new_admin: Address) -> Result<(), PaymentError> {
    let config = config(&env)?;
    config.admin.require_auth();
    
    // Check if transfer already pending
    if env.storage().instance().has(&DataKey::PendingAdmin) {
        return Err(PaymentError::TransferAlreadyPending);
    }
    
    // Store pending admin
    env.storage().instance().set(&DataKey::PendingAdmin, &new_admin);
    extend_instance(&env);
    
    // Emit event
    env.events().publish(
        (symbol_short!("admin"), symbol_short!("proposed")),
        (config.admin.clone(), new_admin.clone())
    );
    
    Ok(())
}

/// Step 2: New admin accepts the role
pub fn accept_admin(env: Env) -> Result<(), PaymentError> {
    let pending = env.storage().instance()
        .get::<DataKey, Address>(&DataKey::PendingAdmin)
        .ok_or(PaymentError::NoPendingTransfer)?;
    
    pending.require_auth();
    
    // Update config with new admin
    let mut config = config(&env)?;
    let old_admin = config.admin.clone();
    config.admin = pending.clone();
    store_config(&env, &config);
    
    // Clear pending transfer
    env.storage().instance().remove(&DataKey::PendingAdmin);
    
    // Emit event
    env.events().publish(
        (symbol_short!("admin"), symbol_short!("changed")),
        (old_admin, pending)
    );
    
    Ok(())
}

/// Cancel a pending admin transfer (callable by current admin)
pub fn cancel_admin_transfer(env: Env) -> Result<(), PaymentError> {
    let config = config(&env)?;
    config.admin.require_auth();
    
    if !env.storage().instance().has(&DataKey::PendingAdmin) {
        return Err(PaymentError::NoPendingTransfer);
    }
    
    env.storage().instance().remove(&DataKey::PendingAdmin);
    
    env.events().publish(
        (symbol_short!("admin"), symbol_short!("cancelled")),
        config.admin
    );
    
    Ok(())
}

/// View pending admin transfer
pub fn get_pending_admin(env: Env) -> Option<Address> {
    env.storage().instance().get(&DataKey::PendingAdmin)
}
```

#### 4. Add Tests

```rust
#[test]
fn two_step_admin_transfer_requires_acceptance() {
    let s = setup();
    let new_admin = Address::generate(&s.env);
    
    // Current admin proposes transfer
    s.pp.transfer_admin(&new_admin);
    
    // Config still shows old admin
    assert_eq!(s.pp.get_config().admin, s.admin);
    
    // New admin accepts
    s.pp.accept_admin();
    
    // Config now shows new admin
    assert_eq!(s.pp.get_config().admin, new_admin);
}

#[test]
fn admin_transfer_can_be_cancelled() {
    let s = setup();
    let new_admin = Address::generate(&s.env);
    
    s.pp.transfer_admin(&new_admin);
    s.pp.cancel_admin_transfer();
    
    assert_eq!(s.pp.get_pending_admin(), None);
    assert_eq!(s.pp.get_config().admin, s.admin);
}

#[test]
fn only_pending_admin_can_accept() {
    let s = setup();
    let new_admin = Address::generate(&s.env);
    let imposter = Address::generate(&s.env);
    
    s.pp.transfer_admin(&new_admin);
    
    // Imposter cannot accept
    assert_eq!(
        s.pp.try_accept_admin(),  // as imposter
        Err(Ok(PaymentError::NotPendingAdmin))
    );
}

#[test]
fn cannot_propose_while_transfer_pending() {
    let s = setup();
    let admin1 = Address::generate(&s.env);
    let admin2 = Address::generate(&s.env);
    
    s.pp.transfer_admin(&admin1);
    
    assert_eq!(
        s.pp.try_transfer_admin(&admin2),
        Err(Ok(PaymentError::TransferAlreadyPending))
    );
}
```

#### 5. Apply Same Pattern to Other Contracts

The same two-step transfer pattern should be applied to:
- merchant-registry
- loyalty-program  
- treasury (if not using multisig governance)
- escrow-service

---

## Issue #63: Fuzz Testing for Payment Amounts

### Current State
Payment amounts are tested only with fixed values (100, 500, 1000 USDC).

### Risk
- Integer overflow in fee calculations
- Edge cases at boundary values (i128::MAX, 0, negative)
- Rounding errors in basis point calculations

### Proposed Solution: Property-Based Testing with `proptest`

#### 1. Add Dependencies to `Cargo.toml`

```toml
[dev-dependencies]
proptest = "1.0"
```

#### 2. Add Property-Based Tests

```rust
#[cfg(test)]
mod fuzz_tests {
    use super::*;
    use proptest::prelude::*;
    
    proptest! {
        #[test]
        fn fee_calculation_never_overflows(
            amount in 1i128..=i128::MAX/10000,  // Safe range for fee calc
            fee_bps in 0u32..=1000u32
        ) {
            // Property: split_fee should never overflow
            let result = split_fee(amount, fee_bps);
            assert!(result.is_some());
            
            if let Some((fee, net)) = result {
                // Property: fee + net = amount
                assert_eq!(fee + net, amount);
                // Property: fee is proportional to fee_bps
                assert!(fee <= amount);
                assert!(net <= amount);
            }
        }
        
        #[test]
        fn payment_round_trip_preserves_amount(
            gross in 1i128..=(1_000_000_000 * 10_000_000),  // Reasonable payment range
            fee_bps in 1u32..=1000u32
        ) {
            let s = setup();
            
            // Pay
            let payment_id = s.pp.pay(
                &s.payer,
                &s.merchant,
                &s.asset,
                &gross,
                &String::from_str(&s.env, "ORDER123")
            );
            
            let payment = s.pp.get_payment(&payment_id);
            
            // Property: stored amount equals input
            assert_eq!(payment.amount, gross);
            
            // Property: fee + net sent = gross
            let (fee, net) = split_fee(gross, fee_bps).unwrap();
            // Verify transfers happened correctly
        }
        
        #[test]
        fn refund_never_exceeds_original_payment(
            amount in 1i128..=(1_000_000_000 * 10_000_000),
            refund_amount in 1i128..=i128::MAX
        ) {
            let s = setup();
            
            // Make payment
            let payment_id = s.pp.pay(&s.payer, &s.merchant, &s.asset, &amount, &String::from_str(&s.env, "ORDER456"));
            
            // Try to refund
            let result = s.pp.try_refund(&payment_id, &refund_amount);
            
            // Property: refund <= original amount OR error
            if refund_amount <= amount {
                assert!(result.is_ok());
            } else {
                assert_eq!(result, Err(Ok(PaymentError::RefundExceedsPayment)));
            }
        }
        
        #[test]
        fn royalty_splits_always_sum_correctly(
            amount in 100i128..=(1_000_000 * 10_000_000),
            split1_bps in 0u32..=5000u32,
            split2_bps in 0u32..=5000u32
        ) {
            // Property: when splits are valid, they sum to ≤ 100%
            let total_bps = split1_bps + split2_bps;
            
            if total_bps <= BPS_DENOMINATOR {
                // Calculate splits
                let split1 = bps_of(amount, split1_bps);
                let split2 = bps_of(amount, split2_bps);
                
                if let (Some(s1), Some(s2)) = (split1, split2) {
                    // Property: splits don't exceed original amount
                    assert!(s1 + s2 <= amount);
                }
            }
        }
    }
}
```

#### 3. Add Makefile Target

```makefile
fuzz:
	PROPTEST_CASES=10000 cargo test --release -- fuzz_tests --ignored

fuzz-quick:
	PROPTEST_CASES=1000 cargo test -- fuzz_tests
```

#### 4. CI Integration

Add to `.github/workflows/ci.yml`:

```yaml
- name: Run fuzz tests
  run: make fuzz-quick
```

---

## Issue #67: Use Events Instead of Storage for Historical Records

### Current State
Every payment is stored in contract storage:
```rust
env.storage().persistent().set(&DataKey::Payment(id), &payment);
```

### Problem
- Storage is expensive on Soroban
- Historical payments accumulate forever
- TTL management required for all historical data
- Indexers can reconstruct history from events

### Proposed Solution: Event-Based History

#### 1. Keep Only Active State in Storage

```rust
// KEEP: Current payment for duplicate detection
env.storage().persistent().set(
    &DataKey::OrderPayment(merchant, order_ref),
    &payment_id
);

// REMOVE: Full payment storage
// env.storage().persistent().set(&DataKey::Payment(id), &payment);
```

#### 2. Emit Comprehensive Events

```rust
#[contractevent]
pub struct PaymentSettled {
    pub id: u64,
    pub payer: Address,
    pub merchant: Address,
    pub settlement: Address,
    pub asset: Address,
    pub gross: i128,
    pub fee: i128,
    pub net: i128,
    pub order_ref: String,
    pub loyalty_points: u64,
    pub timestamp: u64,
}
```

#### 3. Query Functions Return None for Historical Data

```rust
pub fn get_payment(env: Env, id: u64) -> Option<Payment> {
    // Can only query recent payments still in storage
    env.storage().persistent().get(&DataKey::Payment(id))
}

pub fn payment_for_order(env: Env, merchant: Address, order_ref: String) -> Option<u64> {
    // This still works for duplicate detection
    env.storage().persistent().get(&DataKey::OrderPayment(merchant, order_ref))
}
```

#### 4. Document Migration Path

Add to README:
```markdown
### Historical Data

As of v2.0.0, payment history is stored in events rather than contract storage.

**Off-chain indexers** should:
1. Subscribe to `PaymentSettled` events
2. Store in a database (PostgreSQL, etc.)
3. Provide API for historical queries

**On-chain queries** (`get_payment`, `payment_for_order`):
- Only return recent payments (within TTL window)
- Use for duplicate detection, not history
```

#### 5. Gradual Migration

```rust
// Phase 1: Emit events AND store (backward compatible)
pub fn pay_v2(...) {
    // ... existing payment logic ...
    
    // Store for backward compat
    env.storage().persistent().set(&DataKey::Payment(id), &payment);
    
    // Also emit event
    emit_payment_settled(&env, &payment);
}

// Phase 2: Stop storing, only emit
pub fn pay_v3(...) {
    // ... existing payment logic ...
    
    // Only emit event (storage removed)
    emit_payment_settled(&env, &payment);
}
```

#### 6. Update Tests

```rust
#[test]
fn payment_emits_event_with_all_details() {
    let s = setup();
    
    s.pp.pay(&s.payer, &s.merchant, &s.asset, &1000, &String::from_str(&s.env, "ORDER789"));
    
    let events = s.env.events().all();
    let payment_event = events.iter().find(|e| {
        e.topics.contains(&symbol_short!("settled"))
    }).expect("PaymentSettled event");
    
    // Verify event contains all necessary data
    // ... assertions ...
}
```

---

## Implementation Priority

1. **Issue #64 (Two-Step Admin Transfer)** - CRITICAL, implement first
2. **Issue #63 (Fuzz Testing)** - HIGH, good for CI/CD
3. **Issue #67 (Events)** - MEDIUM, requires coordination with backend

## Testing Requirements

All implementations must:
- ✅ Pass existing test suite
- ✅ Add new tests with ≥90% coverage
- ✅ Include integration tests
- ✅ Document breaking changes

## Deployment Considerations

- Issue #64: Backward compatible (adds new functions)
- Issue #63: Dev/test only (no contract changes)
- Issue #67: Breaking change (requires indexer updates)
