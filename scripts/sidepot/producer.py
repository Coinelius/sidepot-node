#!/usr/bin/env python3
"""Produces Sidepot blocks only when one is worth its bid.

Every POLL seconds it asks the node for the game and bids for the next Sidepot block if

  - eCash blocks hold moves or deposits that a Sidepot block would settle (`to_settle` above zero),
  - eCash blocks hold ordinary deposits or withdrawal events for the sidechain (`peg_waiting` above zero),
  - sidechain transactions are waiting in the node's mempool (`mempool_txs` above zero), or
  - the block bonus on offer is larger than the bid.

Otherwise it does nothing, so an idle game costs nothing. A bid is paid only if an eCash miner includes it.

  SIDEPOT_RPC   the node's JSON-RPC            default http://127.0.0.1:6088
  BID_SATS      the bid per block, in sats     default 10000 (the least that earns a block bonus)
  POLL          seconds between looks          default 30
"""
import json
import os
import sys
import time
import urllib.request

RPC = os.environ.get("SIDEPOT_RPC", "http://127.0.0.1:6088")
BID = int(os.environ.get("BID_SATS", "10000"))
POLL = int(os.environ.get("POLL", "30"))
# The rules' block bonus: at most this share of the pool, and at most this many times the bid.
POOL_SHARE, BID_MULTIPLE = 0.05, 2


def rpc(method, *params, timeout=60):
    body = json.dumps({"jsonrpc": "2.0", "id": 0, "method": method, "params": list(params)}).encode()
    req = urllib.request.Request(RPC, body, {"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        reply = json.load(resp)
    if reply.get("error"):
        raise RuntimeError(reply["error"].get("message", str(reply["error"])))
    return reply["result"]


def log(*words):
    print(time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), *words, flush=True)


def reason(state):
    """Why a block is worth bidding for now, or None."""
    if state.get("to_settle"):
        return f"{state['to_settle']} eCash block(s) to settle"
    if state.get("peg_waiting"):
        return f"{state['peg_waiting']} eCash block(s) with sidechain deposits or withdrawal events"
    if state.get("mempool_txs"):
        return f"{state['mempool_txs']} sidechain transaction(s) waiting"
    pool = (state.get("preview") or state)["bonus_pool_sats"]
    bonus = min(int(pool * POOL_SHARE), BID_MULTIPLE * BID)
    if bonus > BID:
        return f"block bonus {bonus} sats for a {BID} sat bid"
    return None


def main():
    log(f"producer started: bid {BID} sats, looking every {POLL} s")
    while True:
        try:
            state = rpc("game_state")
            why = state and reason(state)
            if why:
                before = rpc("getblockcount")
                log("bidding:", why)
                # Returns once the next eCash block has arrived, with or without our block in it.
                rpc("mine", BID, timeout=86400)
                after = rpc("getblockcount")
                log("accepted, Sidepot height", after) if after > before else log("not accepted this time")
        except Exception as error:  # keep going: the node or the enforcer may be restarting
            log("error:", error)
        time.sleep(POLL)


if __name__ == "__main__":
    sys.exit(main())
