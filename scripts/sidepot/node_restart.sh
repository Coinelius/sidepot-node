#!/usr/bin/env bash
# Restarts the regtest Sidepot node on the binary now in the build directory, keeping its data.
set -euo pipefail
DIR=/opt/sidepot-regtest
BIN=/root/sidepot-thunder-build/target/release
CLI="/usr/local/bin/trixie-exec /opt/ecash-1.2.0/bin/bitcoin-cli -datadir=$DIR/node"
[ -f $DIR/sidepot-node.pid ] && kill "$(cat $DIR/sidepot-node.pid)" 2>/dev/null || true
for pid in $(pgrep -x thunder_app || true); do
    grep -qa "$DIR/sidepot-node" /proc/$pid/cmdline && kill $pid || true
done
sleep 3
PK=$(/root/sidepot-build/target/release/sidepot-cli pubkey $(printf '33%.0s' {1..32}))
nohup setsid $BIN/thunder_app --headless --network regtest --datadir $DIR/sidepot-node \
  --mainchain-grpc-url http://127.0.0.1:50153 --l1-rpc-url http://127.0.0.1:18943 \
  --l1-rpc-cookie $DIR/node/regtest/.cookie --game-start-height "$(cat $DIR/sidepot-node.start)" \
  --game-anchor $($CLI getblockhash 105) --producer-key $PK \
  --rpc-addr 127.0.0.1:6088 --net-addr 127.0.0.1:4088 --log-level info >> $DIR/sidepot-node.log 2>&1 </dev/null &
echo $! > $DIR/sidepot-node.pid
sleep 8
echo restarted
