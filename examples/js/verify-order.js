// Merchant backend: confirm an order was paid before releasing goods.
//
//   npm run verify-order
import { addr, env, str, view } from "./common.js";

const processor = env("PAYMENT_PROCESSOR_ID");
const merchant = env("MERCHANT_ADDRESS");
const orderRef = env("ORDER_REF");

try {
  // Any funded account can be the simulation source; it is never charged.
  const payment = await view(
    processor,
    "payment_for_order",
    [addr(merchant), str(orderRef)],
    merchant,
  );
  console.log(`Order ${orderRef}: ${payment.status}`);
  console.log(`  payment id  ${payment.id}`);
  console.log(`  amount      ${payment.amount} (refunded ${payment.refunded})`);
  console.log(`  fee         ${payment.fee}`);
} catch (e) {
  if (String(e).includes("Error(Contract, #9)")) {
    console.log(`Order ${orderRef} has not been paid.`);
  } else {
    throw e;
  }
}
