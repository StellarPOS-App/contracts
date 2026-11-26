# Deployment guide

## 1. Deploy

```bash
make deploy NETWORK=testnet FEE_BPS=100
```

This runs [`scripts/deploy.sh`](../scripts/deploy.sh), which:

1. Creates and funds a `stellarpos-deployer` identity with Friendbot if it does
   not exist (testnet only).
2. Builds and deploys all five contracts.
3. Initializes them with the deployer as admin and a **1-of-1** treasury.
4. Connects loyalty accrual: `loyalty.set_processor(processor)` and
   `processor.set_loyalty(loyalty)`.
5. Writes `deployments/testnet.json`.

Mainnet deploys need `CONFIRM_MAINNET=yes` and an identity you imported
yourself with `stellar keys add`.

## 1b. Verify what is on-chain

```bash
make verify NETWORK=testnet
```

[`scripts/verify.sh`](../scripts/verify.sh) rebuilds every contract from
this checkout, downloads the wasm each contract ID in
`deployments/<network>.json` is running, and compares SHA-256 hashes. It
exits non-zero if anything differs. Each contract also embeds SEP-0055
metadata (`source_repo`, `home_domain`, `desc`) plus the Rust, SDK and CLI
versions it was built with (Soroban SDK pinned to 27.0.6), readable with
`stellar contract info meta --wasm <file>` or `--contract-id <id>`.

Anyone can run this against a published manifest to check that a
deployment runs this source, which is useful before an audit or before
trusting a mainnet deployment.

## 2. Harden the treasury and admin

> **Production requirement:** Never leave a single-key deployer EOA as the
> `admin` of `payment-processor`, `merchant-registry`, `loyalty-program`,
> `escrow-service`, or `payment-stream` on mainnet. Initialize production
> contracts with a multisig contract (such as the hardened `treasury` M-of-N
> multisig) or a Stellar M-of-N multisig account as `admin`.

Before the treasury holds real fees, turn the 1-of-1 deployer setup into a
real multisig:

```bash
T=$(jq -r .contracts.treasury deployments/testnet.json)
ME=$(stellar keys address stellarpos-deployer)

# Add a second signer.
stellar contract invoke --id $T --source stellarpos-deployer --network testnet -- \
  propose --proposer $ME --action '{"AddSigner":"GB...SECOND"}' --memo "add ops signer"
stellar contract invoke --id $T --source stellarpos-deployer --network testnet -- execute --id 1

# Require 2 approvals from now on.
stellar contract invoke --id $T --source stellarpos-deployer --network testnet -- \
  propose --proposer $ME --action '{"SetThreshold":2}' --memo "2-of-2"
stellar contract invoke --id $T --source stellarpos-deployer --network testnet -- execute --id 2
```

## 3. Onboard a merchant

```bash
R=$(jq -r .contracts.merchantRegistry deployments/testnet.json)
# Contract ID of the asset's Stellar Asset Contract (SAC). For native XLM use --asset native.
USDC=$(stellar contract id asset --asset USDC:GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5 --network testnet)

# Merchant signs their own registration.
stellar contract invoke --id $R --source merchant --network testnet -- \
  register --owner $(stellar keys address merchant) --name "Kofi's Coffee" \
  --settlement $(stellar keys address merchant) --accepted_assets "[\"$USDC\"]"

# Admin approves.
stellar contract invoke --id $R --source stellarpos-deployer --network testnet -- \
  approve --owner $(stellar keys address merchant)
```

To enable loyalty for that merchant at 10 points per whole USDC:

```bash
L=$(jq -r .contracts.loyaltyProgram deployments/testnet.json)
stellar contract invoke --id $L --source merchant --network testnet -- \
  enroll --merchant $(stellar keys address merchant) --points_per_unit 10 --unit 10000000
```

## 4. Take a payment

```bash
P=$(jq -r .contracts.paymentProcessor deployments/testnet.json)
stellar contract invoke --id $P --source customer --network testnet -- \
  pay --payer $(stellar keys address customer) --merchant $(stellar keys address merchant) \
  --asset $USDC --amount 50000000 --order_ref "ORD-1001"
```

## 5. Configure the backend

Copy the IDs from `deployments/<network>.json` into the backend's `.env`
(`STELLAR_PAYMENT_PROCESSOR_ID`, and so on). See the backend README.

## Emergency procedures

| Situation | Action |
|---|---|
| Bug in settlement | `payment-processor.pause()` (admin). Checkouts fail fast; no funds are at risk because the processor holds none. |
| Suspected treasury key compromise | Any signer calls `treasury.freeze()`. Then rotate keys through proposals and `Unfreeze` with the full quorum. |
| Bad merchant | `merchant-registry.suspend(owner)`. Their payments are rejected right away. |
| Escrow issue | `escrow-service.pause()`. Sellers can still `refund` buyers while paused. |
