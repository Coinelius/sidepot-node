#!/usr/bin/env bash
# Full cycle against a freshly started Sidepot node (node_start.sh): deposits into game accounts, buys, Sidepot
# blocks mined by BMM, a cashout paid by a Sidepot coinbase, and a comparison with the L1-only replay.
set -euo pipefail
DIR=/opt/sidepot-regtest
SLOT=88
CLI="/usr/local/bin/trixie-exec /opt/ecash-1.2.0/bin/bitcoin-cli -datadir=$DIR/node -rpcwallet=test"
SP=/root/sidepot-build/target/release/sidepot-cli
N="/root/sidepot-thunder-build/target/release/sidepot-node-cli --rpc-url http://127.0.0.1:6088"
MINE="python3 $DIR/regtest_miner.py"
GRPC=127.0.0.1:50153
RPC=http://127.0.0.1:18943; COOKIE=$DIR/node/regtest/.cookie
START=$(cat $DIR/sidepot-node.start)
ANCHOR=$($CLI getblockhash 105)
A_SK=$(printf '11%.0s' {1..32}); B_SK=$(printf '22%.0s' {1..32})
A=$($SP pubkey "$A_SK"); B=$($SP pubkey "$B_SK")

post() {
    local raw funded signed
    raw=$($CLI createrawtransaction '[]' "[{\"data\":\"$1\"}]")
    funded=$($CLI fundrawtransaction "$raw" | python3 -c 'import json,sys; print(json.load(sys.stdin)["hex"])')
    signed=$($CLI signrawtransactionwithwallet "$funded" | python3 -c 'import json,sys; print(json.load(sys.stdin)["hex"])')
    $CLI sendrawtransaction "$signed" >/dev/null
}
deposit() {
    grpcurl -plaintext -d "{\"sidechainId\":$SLOT,\"address\":\"spot:$1\",\"valueSats\":\"100000000\",\"feeSats\":\"10000\"}" \
        $GRPC cusf.mainchain.v1.WalletService/CreateDepositTransaction >/dev/null
}
sign() { local kind=$1 sk=$2; shift 2; $SP "$kind" "$sk" "$ANCHOR" $SLOT "$@" $(( $($SP tick $RPC "$COOKIE" "$START") + 3 )); }
# One L1 block that also BMMs a Sidepot block: request BMM, mine the L1 block that accepts it.
both() {
    $N mine > /tmp/sidepot-mine.out 2>&1 &
    local pid=$!
    sleep 4
    $MINE 1 >/dev/null
    wait $pid || { echo "mine failed: $(cat /tmp/sidepot-mine.out)"; }
}
state() { echo "L1 $($CLI getblockcount) side $($N get-blockcount): $($N game-state | tr -d ' \n')"; }
nonce_of() { $N game-account "$1" | python3 -c 'import json,sys; a=json.load(sys.stdin); print((a or {}).get("nonce",0))'; }

DEST_B58=$($N get-new-address)
DEST=$(python3 - "$DEST_B58" <<'PY'
import sys
al='123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz'
n=0
for c in sys.argv[1]: n=n*58+al.index(c)
print(n.to_bytes(20,'big').hex())
PY
)
echo "players A=${A:0:8} B=${B:0:8}; game starts at L1 $START; cashout address $DEST_B58 ($DEST)"

deposit "$A"; $MINE 1 >/dev/null
deposit "$B"
NA=$(( $(nonce_of "$A") + 1 ))
post "$(sign buy "$A_SK" 1 10 2000000 $NA)"
both; state
post "$(sign buy "$B_SK" 1 5 2000000 1)"
both; state
both; state
echo "A: $($N game-account "$A" | tr -d ' \n')  B: $($N game-account "$B" | tr -d ' \n')"
post "$(sign cashout "$A_SK" 25000000 "$DEST" $((NA + 1)))"
both; both; both; both; state
echo "A: $($N game-account "$A" | tr -d ' \n')"
echo "node wallet balance: $($N balance | tr -d ' \n')"
echo "producer account: $($N game-account $($SP pubkey $(printf '33%.0s' {1..32})) | tr -d ' \n')"
echo "--- L1-only replay"
$SP scan $RPC "$COOKIE" $SLOT 105 "$START" | grep -v "^STATE" | tail -12
