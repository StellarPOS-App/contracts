# Integration examples

Runnable scripts showing how a point-of-sale frontend or backend talks to the
StellarPOS contracts with the JavaScript
[`@stellar/stellar-sdk`](https://github.com/stellar/js-stellar-sdk).

| Script | Who runs it | What it shows |
| --- | --- | --- |
| [`js/checkout.js`](js/checkout.js) | Customer app | Quote the fee, then settle a sale with `pay_with_fee` |
| [`js/verify-order.js`](js/verify-order.js) | Merchant backend | Confirm an order was paid with `payment_for_order` |
| [`js/refund.js`](js/refund.js) | Merchant backend | Refund part of a payment from the settlement account |

## Setup

```sh
cd examples/js
npm install
cp .env.example .env   # then fill in contract IDs and keys
```

Everything is configured through environment variables (see `.env.example`),
so the same scripts run against testnet or a local network. Deploy the
contracts first with `make deploy` (see [`docs/deployment.md`](../docs/deployment.md)).
**Never commit real secret keys.** `.env` is git-ignored.

## The flow

1. **Quote.** `quote(amount)` returns `(fee, net)` for the current fee rate.
   It is a read-only call, so simulate it; no signature or fee is needed.
2. **Pay.** The payer signs `pay_with_fee(payer, merchant, asset, amount,
   order_ref, fee)`. Passing the quoted fee means the payment fails with
   `FeeMismatch` (error 27) instead of charging a different fee if the rate
   changed after the quote. Use plain `pay` if you do not need that guarantee.
3. **Confirm.** The backend calls `payment_for_order(merchant, order_ref)`.
   A payment that does not exist fails with `PaymentNotFound` (error 9).
4. **Refund.** `refund(payment_id, amount, reason)` must be signed by the
   merchant's *settlement* account, since that account holds the funds.

Amounts are in the asset's smallest unit (7 decimals for typical Stellar
assets, so `10_000_000` is one unit). Payment IDs are hashes, not counters:
read the ID from the `pay` return value or look it up by order reference.
Contract errors surface as `Error(Contract, #N)`; the numbers are in
`PaymentError` in
[`contracts/payment-processor/src/lib.rs`](../contracts/payment-processor/src/lib.rs).
