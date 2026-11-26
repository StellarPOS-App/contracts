# Security assumptions

This repository contains Soroban contracts for StellarPOS. Deployments should be
reviewed against the exact commit and built WASM before use.

## Trust model

- The payment processor never custody-holds payer funds; the token contract moves
  funds directly from the payer to the settlement account and treasury.
- Payers, merchants, the registry, and configured integrations are external
  callers. Their authorization and return values are not trusted implicitly.
- The configured admin is trusted to pause the processor and change operational
  parameters. Production deployments should use a multisig or governance
  contract, not a single-key account.
- The merchant registry is trusted to report active merchants and valid checkout
  settlement accounts.
- Accepted token contracts must implement the Soroban token interface correctly.
  Transfers are checked by comparing the recipient balance before and after the
  transfer; a non-crediting transfer is rejected.

## Operational controls

- Payment creation is limited per payer per ledger. The admin can lower or raise
  the limit within the contract's configured maximum.
- Fee changes are capped by `MAX_FEE_BPS` and can be handed to the governance
  council through the governance entrypoints.
- Keep the admin, treasury, registry, token issuers, and deployed WASM hashes in
  the deployment record. Rotate admin authority through a reviewed multisig
  transaction.

## Known limitations

- Merchant analytics are on-chain aggregates of recorded payments. The dispute
  rate remains zero until dispute records are integrated into this processor.
- A pause blocks new payment creation and refunds; operators must verify the
  intended recovery procedure before unpausing.
- Contract events are public. Do not put secrets or personal data in order
  references, categories, callback payloads, or other contract arguments.

## Reporting

Do not disclose a suspected vulnerability in a public issue. Contact the
maintainers privately with the affected contract, network, transaction or
ledger evidence, reproduction steps, and a proposed severity.
