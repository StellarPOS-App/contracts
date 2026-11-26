// Merchant backend: refund part of a payment.
//
//   npm run refund -- <payment-id> <amount> [reason]
//
// Refunds are signed by the merchant's settlement account, which holds the
// funds. The protocol fee is not returned, so the merchant absorbs it.
import { env, i128, invoke, keypair, str, u64, view } from "./common.js";

const [paymentId, amount, reason = "customer request"] = process.argv.slice(2);
if (!paymentId || !amount) {
  console.error("usage: npm run refund -- <payment-id> <amount> [reason]");
  process.exit(1);
}

const processor = env("PAYMENT_PROCESSOR_ID");
const settlement = keypair("SETTLEMENT_SECRET");

const before = await view(processor, "get_payment", [u64(paymentId)], settlement.publicKey());
console.log(`Payment ${paymentId}: ${before.status}, refunded ${before.refunded} of ${before.amount}`);

const totalRefunded = await invoke(
  processor,
  "refund",
  [u64(paymentId), i128(amount), str(reason)],
  settlement,
);
console.log(`Refunded ${amount}. Total refunded: ${totalRefunded}`);
