#!/usr/bin/env bash
# Starts a fresh Sidepot node against the regtest stack; the game starts at the next L1 block.
set -euo pipefail
DIR=/opt/sidepot-regtest
BIN=/root/sidepot-thunder-build/target/release
CLI="/usr/local/bin/trixie-exec /opt/ecash-1.2.0/bin/bitcoin-cli -datadir=$DIR/node"
pkill -f "sidepot-node --headless --network regtest --datadir $DIR/sidepot-node" || true
sleep 1
rm -rf $DIR/sidepot-node && mkdir $DIR/sidepot-node
START=$(( $($CLI getblockcount) + 1 ))
echo $START > $DIR/sidepot-node.start
PK=$(/root/sidepot-build/target/release/sidepot-cli pubkey $(printf '33%.0s' {1..32}))
nohup setsid $BIN/sidepot-node --headless --network regtest --datadir $DIR/sidepot-node \
  --mainchain-grpc-url http://127.0.0.1:50153 --l1-rpc-url http://127.0.0.1:18943 \
  --l1-rpc-cookie $DIR/node/regtest/.cookie --game-start-height $START \
  --game-anchor $($CLI getblockhash 105) --producer-key $PK \
  --rpc-addr 127.0.0.1:6088 --net-addr 127.0.0.1:4088 --log-level info > $DIR/sidepot-node.log 2>&1 </dev/null &
sleep 8
echo "started, game start $START, producer $PK"
