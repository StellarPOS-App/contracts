import {
  Address,
  Contract,
  Keypair,
  TransactionBuilder,
  nativeToScVal,
  rpc,
  scValToNative,
} from "@stellar/stellar-sdk";

export function env(name) {
  const value = process.env[name];
  if (!value) throw new Error(`Missing environment variable ${name}`);
  return value;
}

export const server = () => new rpc.Server(env("RPC_URL"));

// Helpers to build typed arguments for contract calls.
export const addr = (a) => new Address(a).toScVal();
export const i128 = (n) => nativeToScVal(BigInt(n), { type: "i128" });
export const u64 = (n) => nativeToScVal(BigInt(n), { type: "u64" });
export const str = (s) => nativeToScVal(s, { type: "string" });

/** Builds a transaction that calls `method` on `contractId` for `source`. */
async function build(server, contractId, method, args, sourceAddress) {
  const account = await server.getAccount(sourceAddress);
  return new TransactionBuilder(account, {
    fee: "100000",
    networkPassphrase: env("NETWORK_PASSPHRASE"),
  })
    .addOperation(new Contract(contractId).call(method, ...args))
    .setTimeout(60)
    .build();
}

/**
 * Read-only call: simulates the transaction and returns the decoded result.
 * No signature and no fee, so it is safe to call from a browser.
 */
export async function view(contractId, method, args, sourceAddress) {
  const s = server();
  const tx = await build(s, contractId, method, args, sourceAddress);
  const sim = await s.simulateTransaction(tx);
  if (rpc.Api.isSimulationError(sim)) throw new Error(sim.error);
  return scValToNative(sim.result.retval);
}

/**
 * State-changing call: simulates, signs with `keypair`, submits, and waits
 * for the ledger to confirm. Returns the decoded return value.
 */
export async function invoke(contractId, method, args, keypair) {
  const s = server();
  const tx = await build(s, contractId, method, args, keypair.publicKey());
  const prepared = await s.prepareTransaction(tx); // adds footprint + auth
  prepared.sign(keypair);

  const sent = await s.sendTransaction(prepared);
  if (sent.status === "ERROR") throw new Error(JSON.stringify(sent.errorResult));

  let result = await s.getTransaction(sent.hash);
  while (result.status === "NOT_FOUND") {
    await new Promise((r) => setTimeout(r, 1000));
    result = await s.getTransaction(sent.hash);
  }
  if (result.status !== "SUCCESS") throw new Error(`Transaction ${sent.hash} failed`);
  return result.returnValue ? scValToNative(result.returnValue) : undefined;
}

export const keypair = (secretName) => Keypair.fromSecret(env(secretName));
