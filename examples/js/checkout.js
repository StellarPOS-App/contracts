// Customer app: quote the fee, then pay for an order.
//
//   npm run checkout
import { addr, env, i128, invoke, keypair, str, view } from "./common.js";

const processor = env("PAYMENT_PROCESSOR_ID");
const payer = keypair("PAYER_SECRET");
const amount = BigInt(env("AMOUNT"));
const orderRef = env("ORDER_REF");

// 1. Quote: [fee, net] at the current fee rate. Show this to the customer.
const [fee, net] = await view(processor, "quote", [i128(amount)], payer.publicKey());
console.log(`Total ${amount}  =  fee ${fee} + merchant receives ${net}`);

// 2. Pay. The contract recomputes the fee and rejects the payment with
//    FeeMismatch (#27) if it differs from the one the customer was shown.
//    The order reference is unique per merchant, so retrying a double-tapped
//    "Pay" button fails with DuplicateOrder (#7) instead of charging twice.
const paymentId = await invoke(
  processor,
  "pay_with_fee",
  [
    addr(payer.publicKey()),
    addr(env("MERCHANT_ADDRESS")),
    addr(env("ASSET_ID")),
    i128(amount),
    str(orderRef),
    i128(fee),
  ],
  payer,
);
console.log(`Paid. Payment ID: ${paymentId}`);
