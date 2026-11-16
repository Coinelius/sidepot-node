#!/usr/bin/env bash
# Continues after node_e2e2.sh: a player cashes out of the game to a Sidepot coin, and the coin is withdrawn to L1
# through a BIP300 withdrawal bundle.
set -euo pipefail
DIR=/opt/sidepot-regtest
SLOT=88
CLI="/usr/local/bin/trixie-exec /opt/ecash-1.2.0/bin/bitcoin-cli -datadir=$DIR/node -rpcwallet=test"
SP=/root/sidepot-build/target/release/sidepot-cli
N="/root/sidepot-thunder-build/target/release/thunder_app_cli --rpc-url http://127.0.0.1:6088"
MINE="python3 $DIR/regtest_miner.py"
RPC=http://127.0.0.1:18943; COOKIE=$DIR/node/regtest/.cookie
START=$(cat $DIR/sidepot-node.start)
ANCHOR=$($CLI getblockhash 105)
B_SK=$(printf '22%.0s' {1..32}); B=$($SP pubkey "$B_SK")
post() {
    local raw funded signed
    raw=$($CLI createrawtransaction '[]' "[{\"data\":\"$1\"}]")
    funded=$($CLI fundrawtransaction "$raw" | python3 -c 'import json,sys; print(json.load(sys.stdin)["hex"])')
    signed=$($CLI signrawtransactionwithwallet "$funded" | python3 -c 'import json,sys; print(json.load(sys.stdin)["hex"])')
    $CLI sendrawtransaction "$signed" >/dev/null
}
sign() { local kind=$1 sk=$2; shift 2; $SP "$kind" "$sk" "$ANCHOR" $SLOT "$@" $(( $($SP tick $RPC "$COOKIE" "$START") + 3 )); }
both() {
    $N mine --fee-sats 20000 > /tmp/sidepot-mine.out 2>&1 &
    local pid=$!
    sleep 4
    $MINE 1 >/dev/null
    wait $pid || echo "mine failed: $(cat /tmp/sidepot-mine.out)"
}
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
post "$(sign cashout "$B_SK" 100000000 "$DEST" $(( $(nonce_of "$B") + 1 )))"
both; both; both; both
echo "Sidepot wallet after the cashout: $($N balance | tr -d ' \n')"
L1_ADDR=$($CLI getnewaddress)
$N create-withdrawal --amount-sats 50000000 --fee-sats 1000 --mainchain-fee-sats 1000 "$L1_ADDR"
for i in $(seq 60); do
    both
    GOT=$($CLI getreceivedbyaddress "$L1_ADDR" 0)
    if [ "$GOT" != "0.00000000" ]; then
        echo "WITHDRAWN: $GOT ECX arrived at the L1 address after $i more blocks"
        break
    fi
    [ $((i % 10)) = 0 ] && echo "after $i blocks: pending bundle $($N pending-withdrawal-bundle | tr -d ' \n' | cut -c1-160)"
done
echo "Sidepot wallet at the end: $($N balance | tr -d ' \n'); L1 received: $($CLI getreceivedbyaddress "$L1_ADDR" 0)"
