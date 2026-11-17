#!/usr/bin/env bash
# Starts (or restarts) a Sidepot mainnet node that follows and validates the chain. It reads eCash from the local
# full node and the mainnet enforcer. It does not produce blocks unless a producer key is given and the enforcer
# has a wallet.
set -euo pipefail
DIR=${SIDEPOT_MAINNET:-/opt/sidepot-mainnet}
BIN=${SIDEPOT_BIN:-/opt/sidepot-mainnet/bin}
# 6088 and 4088 are the defaults; these leave them to the regtest node on the same host.
RPC_ADDR=${RPC_ADDR:-127.0.0.1:6188}
mkdir -p "$DIR/data"
[ -f "$DIR/node.pid" ] && kill "$(cat "$DIR/node.pid")" 2>/dev/null && sleep 3 || true
nohup setsid "$BIN/thunder_app" --headless --network mainnet --datadir "$DIR/data" \
  --mainchain-grpc-url "${ENFORCER_GRPC:-http://127.0.0.1:50053}" \
  --l1-rpc-url "${L1_RPC:-http://127.0.0.1:8542}" --l1-rpc-cookie "${L1_COOKIE:-/opt/mainnet-full/.cookie}" \
  --rpc-addr "$RPC_ADDR" --private-rpc-addr "$RPC_ADDR" --net-addr "${NET_ADDR:-0.0.0.0:4188}" \
  --log-level info >> "$DIR/node.log" 2>&1 </dev/null &
echo $! > "$DIR/node.pid"
sleep 8
echo "started, pid $(cat "$DIR/node.pid")"
