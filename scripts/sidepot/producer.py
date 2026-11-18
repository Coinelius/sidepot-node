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

Bidding for the bonus against other producers

The bonus for a block is 5% of the bonus pool, but never more than twice the block's bid. So a bid of half the
bonus on offer earns all of it, and anything above half only serves to outbid someone else. Set BONUS_PERCENT to
the most of the bonus you will give up to an eCash miner, and the bid follows the pool:

  BONUS_PERCENT  the highest bid, as a percent of the bonus on offer (1 to 99). It starts at 50, or at
                 BONUS_PERCENT if that is lower, goes up BONUS_STEP points each time another producer's block is
                 accepted, and comes back down a step each time one of yours is.
  BONUS_STEP     points per step                default 10
  MAX_BID_SATS   never bid more than this       default no limit
  PRODUCER_KEY   the account your blocks name   default this node's own game account; set it if the node runs
                                                with --producer-key

With BONUS_PERCENT set, BID_SATS is the least it bids, for blocks that are worth producing without a bonus.
"""
import json
import os
import sys
import time
import urllib.request

RPC = os.environ.get("SIDEPOT_RPC", "http://127.0.0.1:6088")
BID = int(os.environ.get("BID_SATS", "10000"))
POLL = int(os.environ.get("POLL", "30"))
PERCENT = int(os.environ.get("BONUS_PERCENT", "0"))
STEP = int(os.environ.get("BONUS_STEP", "10"))
MAX_BID = int(os.environ.get("MAX_BID_SATS", "0"))
PRODUCER_KEY = os.environ.get("PRODUCER_KEY", "").lower()
# The rules' block bonus: at most this share of the pool and this many times the bid, and none below the least bid.
POOL_BPS, BID_MULTIPLE, MIN_BID = 500, 2, 10_000
# Half the bonus on offer is the bid that earns all of it.
START = min(50, PERCENT)


def bonus_for(pool, bid):
    """The bonus the rules pay a block with this bid."""
    return 0 if bid < MIN_BID else min(pool * POOL_BPS // 10_000, BID_MULTIPLE * bid)


def bonus_bid(pool, level):
    """The bid at `level` percent of the bonus on offer, or 0 if it would not earn more than it costs."""
    bid = max(MIN_BID, pool * POOL_BPS // 10_000 * level // 100)
    if MAX_BID:
        bid = min(bid, MAX_BID)
    return bid if bonus_for(pool, bid) > bid else 0


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


def decide(state, level):
    """(bid, why) if a block is worth bidding for now, or None. `level` is the current percent of the bonus."""
    pool = (state.get("preview") or state)["bonus_pool_sats"]
    chase = bonus_bid(pool, level) if PERCENT else 0
    bid = max(BID, chase)
    bonus = bonus_for(pool, bid)
    earns = f", bonus {bonus} sats" if bonus else ""
    if state.get("to_settle"):
        return bid, f"{state['to_settle']} eCash block(s) to settle{earns}"
    if state.get("peg_waiting"):
        return bid, f"{state['peg_waiting']} eCash block(s) with sidechain deposits or withdrawal events{earns}"
    if state.get("mempool_txs"):
        return bid, f"{state['mempool_txs']} sidechain transaction(s) waiting{earns}"
    if chase:
        return chase, f"block bonus {bonus_for(pool, chase)} sats at {level}% of the bonus on offer"
    if not PERCENT and bonus > BID:
        return BID, f"block bonus {bonus} sats"
    return None


def tip_producer():
    """(hash, producer key in hex) of the newest Sidepot block, or (None, None) before the first."""
    tip = rpc("get_best_sidechain_block_hash")
    if not tip:
        return None, None
    producer = rpc("get_block", tip)["header"]["producer"]
    return tip, producer.lower() if isinstance(producer, str) else bytes(producer).hex()


def main():
    if not 0 <= PERCENT <= 99:
        sys.exit("BONUS_PERCENT must be between 1 and 99")
    log(f"producer started: bid {BID} sats, looking every {POLL} s"
        + (f", up to {PERCENT}% of the block bonus" if PERCENT else ""))
    mine_key, seen, level = PRODUCER_KEY, None, START
    while True:
        try:
            state = rpc("game_state")
            if PERCENT:
                mine_key = mine_key or rpc("game_my_account")["key"].lower()
                # Whose block came in since the last look decides the next bid.
                tip, producer = tip_producer()
                if tip != seen and seen is not None:
                    was = level
                    level = max(START, level - STEP) if producer == mine_key else min(PERCENT, level + STEP)
                    if level != was:
                        whose = "ours" if producer == mine_key else "another producer's"
                        log(f"the last block was {whose}: bidding {level}% of the bonus from here")
                seen = tip
            choice = state and decide(state, level)
            if choice:
                bid, why = choice
                before = rpc("getblockcount")
                log(f"bidding {bid} sats:", why)
                # Returns once the next eCash block has arrived, with or without our block in it.
                rpc("mine", bid, timeout=86400)
                after = rpc("getblockcount")
                log("a block was accepted, Sidepot height", after) if after > before else log("no block accepted this time")
                continue  # look again at once: the next eCash block is already being mined
        except Exception as error:  # keep going: the node or the enforcer may be restarting
            log("error:", error)
        time.sleep(POLL)


if __name__ == "__main__":
    sys.exit(main())
