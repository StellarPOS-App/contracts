# StellarPOS Contracts

Soroban smart contracts for StellarPOS, a point-of-sale system for merchants
who accept payment on the Stellar network.

[![CI](https://github.com/StellarPOS-App/contracts/actions/workflows/ci.yml/badge.svg)](https://github.com/StellarPOS-App/contracts/actions/workflows/ci.yml)
[![Coverage](https://codecov.io/gh/StellarPOS-App/contracts/graph/badge.svg)](https://codecov.io/gh/StellarPOS-App/contracts)
![soroban-sdk](https://img.shields.io/badge/soroban--sdk-27.0.6-blue)
![License](https://img.shields.io/badge/license-MIT-green)

This repository is one of three:

| Repository | What it is |
|---|---|
| [`contracts`](https://github.com/StellarPOS-App/contracts) (this repo) | On-chain settlement, loyalty, treasury and escrow |
| [`backend`](https://github.com/StellarPOS-App/backend) | REST + WebSocket API: catalog, sales, checkout, payment matching |
| [`surface`](https://github.com/StellarPOS-App/surface) | The point-of-sale web app cashiers and managers use |

## Architecture Overview

The StellarPOS system is composed of six smart contracts that work together to provide a complete point-of-sale solution on Stellar:

```mermaid
graph TB
    subgraph External Actors
        CUSTOMER[👤 Customer Wallet]
        MERCHANT[🏪 Merchant]
        ADMIN[🔧 Admin/Council]
        ARBITER[⚖️ Arbiter]
    end

    subgraph Core Settlement
        PP[💳 Payment Processor<br/>settlement & refunds]
        REG[📋 Merchant Registry<br/>approval & assets]
        TREAS[🏦 Treasury<br/>M-of-N multisig]
    end

    subgraph Value-Add Services
        LOY[🎁 Loyalty Program<br/>points & tiers]
        ESC[🤝 Escrow Service<br/>held payments & disputes]
        STREAM[⏱️ Payment Stream<br/>per-second flows]
    end

    subgraph Infrastructure
        TOKEN[🪙 Stellar Asset<br/>SAC token]
        VAULT[📦 Yield Vault<br/>optional DeFi]
    end

    %% Customer interactions
    CUSTOMER -->|pay/pay_batch| PP
    CUSTOMER -->|create/release| ESC
    CUSTOMER -->|create stream| STREAM
    CUSTOMER -->|redeem points| LOY

    %% Merchant interactions
    MERCHANT -->|register| REG
    MERCHANT -->|refund| PP
    MERCHANT -->|claim escrow| ESC
    MERCHANT -->|enroll customers| LOY
    MERCHANT -->|co-sign redemption| LOY

    %% Admin interactions
    ADMIN -->|approve/suspend| REG
    ADMIN -->|configure| PP
    ADMIN -->|propose/approve| TREAS
    ADMIN -->|pause contracts| PP
    ADMIN -->|pause contracts| REG
    ADMIN -->|pause contracts| LOY
    ADMIN -->|pause contracts| ESC

    %% Arbiter interactions
    ARBITER -->|resolve dispute| ESC

    %% Core contract interactions
    PP -->|accepts?<br/>settlement_of?| REG
    PP -->|accrue points| LOY
    PP -->|fee transfer| TREAS
    PP -->|transfer net| TOKEN
    PP -->|transfer fee| TOKEN

    %% Escrow flows
    ESC -->|lock/release funds| TOKEN
    ESC -.->|independent| PP

    %% Stream flows
    STREAM -->|withdraw/cancel| TOKEN

    %% Treasury operations
    TREAS -->|withdraw| TOKEN
    TREAS -->|deposit/withdraw| VAULT
    VAULT -->|yield| TOKEN

    %% Styling
    classDef customer fill:#e1f5ff,stroke:#0288d1,stroke-width:2px
    classDef merchant fill:#fff9c4,stroke:#f57f17,stroke-width:2px
    classDef admin fill:#f3e5f5,stroke:#7b1fa2,stroke-width:2px
    classDef core fill:#c8e6c9,stroke:#388e3c,stroke-width:3px
    classDef service fill:#ffe0b2,stroke:#e64a19,stroke-width:2px
    classDef infra fill:#f5f5f5,stroke:#616161,stroke-width:2px
    
    class CUSTOMER customer
    class MERCHANT merchant
    class ADMIN,ARBITER admin
    class PP,REG,TREAS core
    class LOY,ESC,STREAM service
    class TOKEN,VAULT infra
```

### Key Relationships

- **Payment Processor** is the orchestrator: it validates merchants through the registry, transfers funds via tokens, sends fees to treasury, and accrues loyalty points
- **Merchant Registry** is the source of truth for who can receive payments and which assets they accept
- **Treasury** is a fully independent M-of-N multisig that receives protocol fees and can deploy idle funds to yield vaults
- **Loyalty Program** maintains per-merchant customer accounts with automatic tier upgrades (Bronze → Silver → Gold → Platinum)
- **Escrow Service** operates independently for held payments with dispute resolution
- **Payment Stream** enables per-second continuous payments for subscriptions and payroll

All settlement happens atomically in a single transaction—either the entire payment succeeds (fees to treasury, net to merchant, points accrued) or nothing moves.

## The contracts

| Crate | Purpose | Key entry points |
|---|---|---|
| [`merchant-registry`](contracts/merchant-registry/src/lib.rs) | Who may get paid, where funds settle, which assets each merchant takes | `register`, `approve`, `suspend`, `accepts`, `settlement_of` |
 fix/issue-165-add-payment-splitting
| [`payment-processor`](contracts/payment-processor/src/lib.rs) | Atomic sale settlement: fee to treasury, net to merchant, one payment per order | `pay`, `pay_batch`, `refund`, `payment_for_order`, `quote` |

| [`payment-processor`](contracts/payment-processor/src/lib.rs) | Atomic sale settlement: fee to treasury, net to merchant, one payment per order | `pay`, `pay_batch`, `pay_with_royalties`, `refund`, `payment_for_order`, `quote`, `propose_param_change` |
 main
| [`loyalty-program`](contracts/loyalty-program/src/lib.rs) | Per-merchant points with automatic Bronze/Silver/Gold/Platinum tiers | `enroll`, `accrue`, `redeem`, `get_account` |
| [`treasury`](contracts/treasury/src/lib.rs) | M-of-N multisig that holds protocol fees and can put idle funds into approved yield vaults | `propose`, `approve`, `execute`, `freeze`, `vaults` |
| [`escrow-service`](contracts/escrow-service/src/lib.rs) | Held payments for deliveries and pre-orders, with dispute resolution | `create`, `release`, `claim`, `raise_dispute`, `resolve`, `resolve_by_default` |
| [`payment-stream`](contracts/payment-stream/src/lib.rs) | Per-second payment streams for subscriptions, rent and payroll; cancel splits earned and unearned exactly | `create`, `withdraw`, `cancel`, `available` |
| [`shared`](contracts/shared/src/lib.rs) | Library, not deployed: basis-point fee math and storage TTL helpers | `bps_of`, `split_fee`, `extend_*` |

## Contract Architecture

The following diagram shows how the contracts interact with each other and the core entities:

```
                                    ┌─────────────────┐
                                    │   Customer      │
                                    │   Wallet        │
                                    └────────┬────────┘
                                             │
                    ┌────────────────────────┼────────────────────────┐
                    │                        │                        │
                    ▼                        ▼                        ▼
         ┌──────────────────┐    ┌──────────────────┐    ┌──────────────────┐
         │  payment-        │    │  escrow-         │    │  payment-        │
         │  processor       │    │  service         │    │  stream          │
         └────────┬─────────┘    └──────────────────┘    └──────────────────┘
                  │                       │
         ┌────────┼───────────┐          │ (disputes)
         │        │           │          │
         ▼        ▼           ▼          ▼
    ┌────────┐ ┌──────┐  ┌────────┐  ┌────────┐
    │merchant│ │Token │  │loyalty-│  │arbiter │
    │registry│ │ SAC  │  │program │  │ (EOA)  │
    └────────┘ └──────┘  └────────┘  └────────┘
         │         │          │
         │         ▼          │
         │    ┌──────────┐   │
         └───▶│ treasury │◀──┘
              │(multisig)│
              └──────────┘
                    │
                    ▼
           ┌─────────────────┐
           │  Protocol Fees  │
           │  & Yield Vaults │
           └─────────────────┘

Legend:
  ─────▶  Direct contract calls / queries
  │       Data flow / dependency
  
Key Relationships:
  • payment-processor: checks merchant-registry, moves tokens, accrues loyalty points
  • escrow-service: holds funds, calls arbiter for disputes
  • loyalty-program: tracks customer points per merchant
  • treasury: receives protocol fees, controlled by multisig
  • merchant-registry: authorizes who can receive payments
  • payment-stream: continuous per-second payments (subscriptions, payroll)
```

### How a sale settles

```mermaid
sequenceDiagram
    participant C as Customer wallet
    participant P as payment-processor
    participant R as merchant-registry
    participant T as Token (SAC)
    participant L as loyalty-program
    C->>P: pay(payer, merchant, asset, amount, order_ref)
    P->>R: accepts(merchant, asset)?
    R-->>P: true
    P->>R: settlement_of(merchant)
    P->>T: transfer(payer -> settlement, net)
    P->>T: transfer(payer -> treasury, fee)
    P->>L: accrue(processor, merchant, payer, amount)
    P-->>C: payment id (event: payment_settled)
```

All transfers happen inside one transaction, so either the whole sale settles
or nothing moves. The processor never holds customer funds, and the
`(merchant, order_ref)` pair is unique, so a retried checkout cannot charge
twice.

### Design rules

These hold across every contract:

- **Typed errors, no panics on user input.** Each contract has a
  `#[contracterror]` enum. Clients get a stable numeric code for every
  rejection.
- **Events for every state change**, defined with `#[contractevent]`, so the
  backend and indexers can follow everything without polling storage.
- **Emergency pause.** Registry, processor, loyalty and escrow can be paused
  by their admin. Escrow refunds still work while paused, because returning
  funds to a buyer is always safe. The treasury uses a multisig freeze
  instead.
- **Checked arithmetic.** Fee and split math goes through `shared::bps_of`,
  which returns `None` on overflow or on more than 100%.
- **TTL management.** Every read or write of a long-lived record extends its
  TTL, so active data is never archived.

More detail on each contract, including state machines and storage layout, is
in [docs/architecture.md](docs/architecture.md).

## Glossary

Point-of-sale terms used across the contracts and docs:

| Term | Meaning |
|---|---|
| **Merchant** | A business registered in `merchant-registry`. Only approved, active merchants can be paid. |
| **Settlement account** | The address a merchant's sales are paid into (can differ from the merchant's own address). Refunds are sent from it. |
| **Order reference (`order_ref`)** | The POS order number. Each merchant can be paid once per order reference, so a double-tapped "Pay" never charges twice. |
| **Settlement** | Completing a sale on-chain: the fee goes to the treasury and the net goes to the settlement account, in one transaction. |
| **Gross / net amount** | Gross is what the customer pays; net is what remains after the protocol fee. |
| **Basis points (bps)** | Hundredths of a percent: 100 bps = 1%, 10,000 bps = 100%. Fees and splits are set in bps. |
| **Protocol fee (`fee_bps`)** | The platform's cut of each sale, capped at 10% (`MAX_FEE_BPS`), paid to the treasury. |
| **Royalty split** | Extra recipients (artists, referrers) paid a bps share of the net amount on a marketplace sale; the seller gets the rest. |
| **Quote** | A preview of the fee/net split for an amount, shown at checkout before paying. |
| **Batch payment** | Several orders paid by one customer in one all-or-nothing transaction (`pay_batch`). |
| **Refund (partial / full)** | Money sent back from the settlement account to the payer, up to the gross amount. Fees and royalties are not returned. |
| **Treasury** | The M-of-N multisig contract that holds protocol fees. |
| **Escrow** | A payment held by the contract until delivery: released by the buyer or claimable by the seller after `release_at`. |
| **Release time (`release_at`)** | When escrowed funds become claimable by the seller; buyers can open a dispute until then. |
| **Dispute / arbitration window** | A dispute freezes an escrow; the arbiter must rule within the arbitration window or a default rule resolves it. |
| **Payment stream** | Money paid per second (subscriptions, rent, payroll); cancelling splits earned and unearned amounts exactly. |
| **Loyalty tier** | Bronze, Silver, Gold or Platinum: a customer's level with a merchant, which boosts points earned. |
| **Governance council** | Once enabled, the members who must approve fee-rate and treasury changes instead of the admin alone. |
| **TTL / bump** | Soroban storage expires unless extended; contracts bump records whenever they are read or written. |

Escrow seller payouts have a one-hour minimum lock by default. The admin can
set a longer minimum with `set_min_lock_period(seconds)`. New escrows record the
policy at funding and require `release_at` at or after the lock ends. Buyer-only
refunds can return funds before unlock. Each escrow pays out in the asset it
recorded at funding. Merchant registration leaves payment statistics absent
until the first successful payment. The payment count reads as zero in the meantime.


## Getting started

### Prerequisites

- Rust stable with the `wasm32v1-none` target (pinned in
  [`rust-toolchain.toml`](rust-toolchain.toml), installed automatically by
  rustup)
- [Stellar CLI](https://developers.stellar.org/docs/tools/cli) 23 or newer,
  for building optimized wasm and deploying

### Build and test

```bash
git clone https://github.com/StellarPOS-App/contracts.git
cd contracts

make test      # 63 tests across all crates
make check     # rustfmt + clippy -D warnings + tests (what CI runs)
make build     # optimized wasm in target/wasm32v1-none/release/
make sizes     # wasm sizes, all well under the 64 KB limit
make verify    # check a deployment runs exactly this source (NETWORK=testnet)
```

### Deploy to testnet

```bash
make deploy                      # NETWORK=testnet, FEE_BPS=100 by default
NETWORK=testnet FEE_BPS=250 make deploy
```

[`scripts/deploy.sh`](scripts/deploy.sh) creates and funds a deployer
identity if needed, deploys all five contracts, initializes them, connects
the loyalty program to the processor, and writes every contract ID to
`deployments/<network>.json`. See [docs/deployment.md](docs/deployment.md)
for the post-deploy checklist (multisig rotation, merchant onboarding) and
example `stellar contract invoke` calls.

## Project layout

```
contracts/
  shared/               fee math and TTL helpers (rlib)
  merchant-registry/
  payment-processor/
  loyalty-program/
  treasury/
  escrow-service/
    src/dispute.rs      dispute reasons, role rules, default rulings
    tests/              end-to-end dispute scenarios
docs/                   architecture, deployment and authorization audit
examples/js/            JavaScript integration examples (checkout, verify, refund)
scripts/deploy.sh       deploy + initialize + wire
deployments/            contract IDs per network (written by deploy.sh)
```

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md). In short: open an issue first for
anything non-trivial, keep pull requests focused, and run `make check`
before pushing. Security issues go through [SECURITY.md](SECURITY.md), never
public issues.

## License

[MIT](LICENSE)

 fix/issue-165-add-payment-splitting
## Group Payment Splitting\n\nA pay_split entry point allowing multiple payers to contribute to a single order atomically, reverting fully if any sub-transfer fails.

 fix/issue-164-fix-admin-revocation-cleanup
## Admin Revocation Cleanup\n\nRevoking an admin must cancel all pending multisig proposals requiring that admin's signature to prevent orphaned votes.

 fix/issue-163-fix-payment-id-predictability
## Payment ID Uniqueness\n\nPayment IDs from predictable inputs allow a griefing attacker to occupy an ID slot. Use a hash including an unpredictable caller-supplied nonce.

 fix/issue-162-add-malicious-token-tests
## Malicious Token Testing\n\nThe test suite must include a mock token returning false from transfer or reverting mid-transfer to test behaviour against non-standard SAC implementations.

 fix/issue-161-add-compliance-report-hook
## Compliance Report Hook\n\nAn optional on-chain hook address per merchant enables the processor to emit compliance events after each settlement without modifying core contract logic.

 fix/issue-160-fix-merchant-payment-count-on-refund
## Merchant Payment Count on Refund\n\nThe payment_count metric must be decremented or a refund_count incremented when a refund is processed to avoid inflating reported transaction volume.

 fix/issue-158-add-pos-terms-glossary
## POS Terms Glossary\n\nA glossary in docs/glossary.md (settlement, escrow, loyalty tier, BPS, SAC, order_ref) helps new contributors understand contract semantics without prior Stellar knowledge.

## Configurable Royalty Split\n\nMarketplace integrations need a royalty split where a configurable percentage forwards to a creator address, stored per merchant and enforced atomically within the pay entry point.
 main
 main
 main
 main
 main
 main
 main
