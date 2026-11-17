#!/usr/bin/env bash
# Second cycle against a freshly started Sidepot node: a qualifying BMM bid earns the block bonus, a block is
# invalidated and the game rolls back, one Sidepot block catches up a long run of L1 blocks, and the round ends
# with a jackpot.
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
A_SK=$(printf '11%.0s' {1..32}); B_SK=$(printf '22%.0s' {1..32}); P_SK=$(printf '33%.0s' {1..32})
A=$($SP pubkey "$A_SK"); B=$($SP pubkey "$B_SK"); P=$($SP pubkey "$P_SK")
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
both() {
    $N mine --fee-sats 20000 > /tmp/sidepot-mine.out 2>&1 &
    local pid=$!
    sleep 4
    $MINE 1 >/dev/null
    wait $pid || echo "mine failed: $(cat /tmp/sidepot-mine.out)"
}
gs() { $N game-state | tr -d ' \n'; }
state() { echo "L1 $($CLI getblockcount) side $($N get-blockcount): $(gs)"; }
acct() { $N game-account "$1" | tr -d ' \n'; }
nonce_of() { $N game-account "$1" | python3 -c 'import json,sys; a=json.load(sys.stdin); print((a or {}).get("nonce",0))'; }
field() { python3 -c "import json,sys; print(json.load(sys.stdin)['$1'])"; }

deposit "$A"; $MINE 1 >/dev/null
deposit "$B"
post "$(sign buy "$A_SK" 1 100 2000000 $(( $(nonce_of "$A") + 1 )))"
both; both; both; both; both; state
echo "BONUS producer account: $(acct "$P")"

echo "--- rollback"
BEFORE=$(gs); H=$($N get-blockcount)
post "$(sign buy "$B_SK" 1 5 2000000 $(( $(nonce_of "$B") + 1 )))"
both; both; both
MID=$(gs)
VICTIM=$($N get-block-hash $H | tr -d '"')
$N invalidate-block "$VICTIM" >/dev/null
AFTER=$(gs)
echo "before:      $BEFORE"
echo "3 blocks on: $MID"
echo "invalidated: $AFTER (side $($N get-blockcount))"
[ "$BEFORE" = "$AFTER" ] && echo "ROLLBACK OK: the game is back where block $((H-1)) left it" || echo "ROLLBACK MISMATCH"

echo "--- catch-up over a long run of L1 blocks, to the end of the round"
for _ in $(seq 150); do
    $CLI sendtoaddress "$($CLI getnewaddress)" 0.001 >/dev/null
    $MINE 1 >/dev/null
done
both; both; both; state
echo "A: $(acct "$A")  B: $(acct "$B")  producer: $(acct "$P")"
echo "--- L1-only replay"
$SP scan $RPC "$COOKIE" $SLOT 105 "$START" | grep -v "^STATE" | grep -v ": bmm \|: deposit " | tail -8
