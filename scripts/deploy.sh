#!/usr/bin/env bash
# Deploys and wires the full StellarPOS contract suite.
#
#   NETWORK=testnet SOURCE=stellarpos-deployer FEE_BPS=100 ./scripts/deploy.sh
#
# The SOURCE identity becomes the admin of every contract and the single
# signer of a 1-of-1 treasury. Rotate the treasury to real multisig signers
# (AddSigner + SetThreshold proposals) before handling real funds.
#
# Contract IDs are written to deployments/$NETWORK.json for the backend.

set -euo pipefail

NETWORK="${NETWORK:-testnet}"
SOURCE="${SOURCE:-stellarpos-deployer}"
FEE_BPS="${FEE_BPS:-100}"
WASM_DIR="target/wasm32v1-none/release"
OUT="deployments/${NETWORK}.json"

cd "$(dirname "$0")/.."

if [[ "$NETWORK" == "mainnet" && "${CONFIRM_MAINNET:-}" != "yes" ]]; then
  echo "Refusing to deploy to mainnet without CONFIRM_MAINNET=yes" >&2
  exit 1
fi

if ! stellar keys address "$SOURCE" >/dev/null 2>&1; then
  if [[ "$NETWORK" == "mainnet" ]]; then
    echo "Identity '$SOURCE' not found. Import it with: stellar keys add $SOURCE" >&2
    exit 1
  fi
  echo "Creating and funding identity '$SOURCE' on $NETWORK"
  stellar keys generate "$SOURCE" --network "$NETWORK" --fund
fi
ADMIN="$(stellar keys address "$SOURCE")"
echo "Admin: $ADMIN"

# Always rebuild: deploying a stale wasm from an earlier build would put
# different code on-chain than this checkout.
stellar contract build >/dev/null 2>&1

deploy() {
  stellar contract deploy \
    --wasm "$WASM_DIR/$1.wasm" \
    --source "$SOURCE" \
    --network "$NETWORK"
}

invoke() {
  local id="$1"; shift
  stellar contract invoke --id "$id" --source "$SOURCE" --network "$NETWORK" -- "$@"
}

echo "Deploying contracts"
REGISTRY="$(deploy merchant_registry)"
TREASURY="$(deploy treasury)"
PROCESSOR="$(deploy payment_processor)"
LOYALTY="$(deploy loyalty_program)"
ESCROW="$(deploy escrow_service)"
STREAMS="$(deploy payment_stream)"

echo "Initializing"
invoke "$REGISTRY"  initialize --admin "$ADMIN"
invoke "$TREASURY"  initialize --signers "[\"$ADMIN\"]" --threshold 1
invoke "$PROCESSOR" initialize --admin "$ADMIN" --registry "$REGISTRY" --treasury "$TREASURY" --fee_bps "$FEE_BPS"
invoke "$LOYALTY"   initialize --admin "$ADMIN"
invoke "$ESCROW"    initialize --admin "$ADMIN"
invoke "$STREAMS"   initialize --admin "$ADMIN"

echo "Wiring loyalty accrual into the payment processor"
invoke "$LOYALTY"   set_processor --processor "$PROCESSOR"
# Option<Address> arguments are JSON, so the ID needs quotes.
invoke "$PROCESSOR" set_loyalty --loyalty "\"$LOYALTY\""

mkdir -p deployments
cat > "$OUT" <<JSON
{
  "network": "$NETWORK",
  "deployedAt": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "admin": "$ADMIN",
  "feeBps": $FEE_BPS,
  "contracts": {
    "merchantRegistry": "$REGISTRY",
    "paymentProcessor": "$PROCESSOR",
    "loyaltyProgram": "$LOYALTY",
    "treasury": "$TREASURY",
    "escrowService": "$ESCROW",
    "paymentStream": "$STREAMS"
  }
}
JSON

echo "Done. Contract IDs written to $OUT"
cat "$OUT"
