# Contract Upgrade Procedures

This document describes the safe procedure for upgrading contract code on Soroban, including pre-upgrade checks, migration steps, and rollback procedures for StellarPOS contracts.

## Overview

Soroban contracts are immutable once deployed. Upgrades require:
1. Deploying a new contract binary
2. Migrating state from the old contract to the new one (if needed)
3. Updating references in dependent systems
4. Performing rollback if issues occur during or after deployment

## Pre-Upgrade Checklist

Before initiating an upgrade, ensure:

- [ ] All changes have been tested in a test environment matching production
- [ ] Contract state has been backed up or dumped to external storage
- [ ] The upgrade path has been reviewed by at least one team member
- [ ] Dependent contracts have been identified and are ready to migrate
- [ ] A communication plan is in place for affected merchants and users
- [ ] TTL (time-to-live) for affected storage entries has been verified
- [ ] Gas estimation has been performed for all state migration transactions
- [ ] Rollback plan is prepared and tested

### State Backup

Before upgrading, dump critical state from the old contract:

```bash
# Example: Backup payment-processor state
soroban contract invoke \
  --id <CONTRACT_ID> \
  --operation get_payment \
  --operation-arg <PAYMENT_ID> \
  > backup.json
```

## Migration Strategy

### 1. Deploy New Contract

Deploy the new contract binary to the same network:

```bash
soroban contract deploy \
  --network testnet \
  --source-account <ACCOUNT> \
  --wasm target/wasm32-unknown-unknown/release/contract_name.wasm
```

### 2. Initialize New Contract

Initialize the new contract with appropriate parameters:

```bash
soroban contract invoke \
  --network testnet \
  --id <NEW_CONTRACT_ID> \
  --operation initialize \
  --operation-arg <ARGS>
```

### 3. Migrate State

For contracts that maintain persistent storage, migrate relevant state:

#### Payment Processor Example

```bash
# 1. Query all payments from old contract
# 2. For each payment, call set methods on new contract
# 3. Verify record counts match

PAYMENT_COUNT=$(soroban contract invoke \
  --id <OLD_CONTRACT_ID> \
  --operation payment_count)

echo "Total payments to migrate: $PAYMENT_COUNT"
```

#### Critical Migration Considerations

- **No Data Loss**: Verify record counts before and after migration
- **Storage TTL**: Call `extend_persistent` for all migrated entries to reset their TTL
- **Ordering**: Process records in a deterministic order (e.g., by ID)
- **Batching**: For large datasets, migrate in batches to avoid transaction size limits
- **Validation**: After migration, spot-check random records for correctness

### 4. Update Contract References

Update all dependent systems to use the new contract address:

- **Configuration Files**: Update network-specific config files with new contract IDs
- **Registry Entries**: For payment-processor, ensure merchant-registry points to the new address
- **Backend Services**: Update REST API and WebSocket services
- **Monitoring**: Update alerting and observability tools to track the new contract

## Verification Steps

After migration, verify the upgrade was successful:

```bash
# 1. Verify config matches expectations
soroban contract invoke \
  --id <NEW_CONTRACT_ID> \
  --operation get_config

# 2. Verify record counts
soroban contract invoke \
  --id <NEW_CONTRACT_ID> \
  --operation payment_count

# 3. Spot-check specific records
soroban contract invoke \
  --id <NEW_CONTRACT_ID> \
  --operation get_payment \
  --operation-arg 1

# 4. Run test transactions
# - For payment-processor: Test a sample payment
# - For merchant-registry: Test a lookup
# - For escrow-service: Test an escrow creation
```

## Handling TTL (Time-to-Live) Issues

Soroban stores data with expiration dates. When migrating state:

- **Instance Storage**: Bumped to ~30 days on every contract operation
- **Persistent Storage**: Bumped to ~90 days on every read or write

**Important**: After migrating records, explicitly extend their TTL:

```rust
// In migration code, after writing each record:
extend_persistent(env, &key);
```

Failure to extend TTL means migrated records may expire unexpectedly.

## Rollback Procedure

If issues are detected after upgrade, follow these steps:

### 1. Identify the Issue

- Monitor contract event logs for errors
- Check backend service logs for transaction failures
- Verify user reports of system issues

### 2. Immediate Actions

- **Pause the new contract**: If it has a pause function, invoke it immediately
- **Revert configuration**: Point systems back to the old contract address
- **Alert team**: Notify all stakeholders of the issue and rollback

### 3. Restore from Backup

```bash
# 1. Determine what state needs to be restored
# 2. Reinitialize the old contract if needed
# 3. Restore state from backup

# Example for payment-processor
soroban contract invoke \
  --id <OLD_CONTRACT_ID> \
  --operation unpause  # If it was paused during upgrade
```

### 4. Root Cause Analysis

- Analyze what went wrong
- Review the new contract code for the bug
- Plan a fixed upgrade that addresses the issue

### 5. Retry Upgrade (Once Fixed)

Once the issue is resolved:
1. Deploy the fixed contract
2. Repeat the migration process
3. Run enhanced verification tests

## Emergency Contacts

- **Contract Lead**: [Team contact]
- **DevOps**: [Team contact]
- **On-Call**: [On-call rotation process]

## Network-Specific Details

### Testnet

- Faster finality: ~2 blocks (10 seconds)
- TTL cycles faster: Test contracts expire in ~1-2 hours
- Good for testing migration procedures

### Mainnet

- Slower finality: ~5+ blocks (25+ seconds)
- Longer TTL: Records live for 90 days between touches
- Requires careful coordination with merchants

## Automated Upgrade Tools

For large-scale migrations, consider:

1. **State Dump Scripts**: Automate dumping state from old contract
2. **Migration Scripts**: Automate state transfer to new contract
3. **Verification Scripts**: Automate spot-checking and record counts
4. **Monitoring**: Alert if record counts diverge or TTL issues occur

## Related Documentation

- [Architecture Overview](./architecture.md) - Contract design and dependencies
- [Deployment Guide](./deployment.md) - Initial contract deployment
- [Shared Library](../contracts/shared/src/lib.rs) - TTL management utilities (`extend_persistent`, `extend_instance`)


## In-place escrow upgrades and schema migration

The escrow service exposes `migrate()` and `upgrade(wasm_hash)`. The upgrade entry point is admin-authenticated and always runs the storage migration hook before calling Soroban's `update_current_contract_wasm`, so a wasm replacement cannot bypass the migration step. `schema_version()` exposes the on-chain version for deployment verification. Future storage-layout changes should extend `migrate_storage` with the next append-only schema transition before raising `CURRENT_SCHEMA_VERSION`.
