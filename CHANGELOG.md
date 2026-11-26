# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- `payment-processor`: on-chain payment receipt generation with queryable receipts by payment ID (#66).
- `merchant-registry`: support for multiple accepted token types per merchant via configurable whitelist (#62).

### Fixed
- `shared`: floor computed basis-point fees at 0, so a negative fee can never
  be deducted and overpay a recipient (#169).
- `escrow-service`: enforce escrow expiry check in claim function to prevent settlements after agreed window (#65).

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
