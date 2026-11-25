# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- `payment-processor`: `pay_with_fee` takes a pre-computed fee and rejects the
  payment with `FeeMismatch` if it differs from the on-chain computation (#121).
- `payment-processor`: `set_merchant_callback`, gated by the merchant's
  authorization (#118).
- `examples/`: JavaScript integration scripts for checkout, order
  verification and refunds (#86).
- `docs/authorization-audit.md`: audit of `require_auth` across all entry
  points (#118).

### Changed
- `payment-processor`: payment IDs are a hash of (payer, merchant, amount,
  timestamp, nonce) with a monotonic nonce that refunds never rewind, so two
  payments in one ledger cannot collide (#88).

### Fixed
- `payment-processor`: `pay_delegated` now authenticates the delegatee instead
  of the contract's own address (#118).
- Repaired `main`, which did not compile: unresolved merge debris in
  `payment-processor`, and stale `loyalty-program` and `escrow-service` tests.
- `shared`: floor computed basis-point fees at 0, so a negative fee can never
  be deducted and overpay a recipient (#169).

## [0.2.0]

The first version that compiles, tests, and deploys.

### Added
- `merchant-registry`: registration, approval, suspension, accepted-asset lists.
- `payment-processor`: registry-checked settlement, order-reference idempotency,
  treasury fees, settlement-authorized refunds, optional loyalty accrual.
- `loyalty-program`: per-merchant points with automatic tiers.
- `treasury`: M-of-N multisig with proposals, expiry, and an emergency freeze.
- `escrow-service`: time-locked release, partial release, disputes with
  arbiter rulings and per-reason default rulings.
- `shared`: checked basis-point math and TTL helpers.
- 63 tests, a CI workflow, `scripts/deploy.sh`, and architecture and deployment docs.

### Fixed
- The Cargo workspace did not resolve because two crates declared their own
  `[workspace]` (#16).

### Removed
- Committed `target/` build output (about 160 MB) and the commit-generation script.
