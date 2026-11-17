# Sidepot node

The node for Sidepot, a last-buyer-wins key game in BIP300 slot 88 on eCash. It is a fork of
[Thunder](https://github.com/LayerTwo-Labs/thunder-rust): a UTXO sidechain with blind merged mining and the BIP300
peg, with the game's state added to consensus.

Game moves are not sidechain transactions. A move is a signed entry in an `OP_RETURN` output of an ordinary eCash
transaction, and every Sidepot block must process all of them, in eCash block order, so a block producer cannot
leave a move out. The rules are in the [`sidepot`](https://github.com/Coinelius/sidepot) library, which this node
links, and are written up at <https://sidepot.cash/rules>.

The software is new and has not been audited.

## Who needs to run it

Players do not: a wallet and <https://sidepot.cash> are enough. Run a node to check the game yourself or to produce
Sidepot blocks.

## What it needs beside it

- **An eCash node** with its JSON-RPC reachable, unpruned and with `txindex=1`. The Sidepot node reads each block's
  transactions and fees from it (`getblock <hash> 3`).
- **A [BIP300/301 enforcer](https://github.com/LayerTwo-Labs/bip300301_enforcer)** on that eCash node, started with
  `--fork=ecash`. Following the chain needs only its gRPC address. Producing blocks also needs its wallet
  (`--enable-wallet`) with some ECX in it to pay bids to miners.

## Running

The release archives hold two programs: `sidepot-node` (the node, with a desktop window unless `--headless`) and
`sidepot-node-cli` (commands for a running node).

```bash
sidepot-node --headless \
  --mainchain-grpc-url http://127.0.0.1:50051 \
  --l1-rpc-url http://127.0.0.1:8332 \
  --l1-rpc-cookie /path/to/ecash/.cookie
```

| Flag | Meaning | Default |
|---|---|---|
| `--network` | `mainnet` or `regtest` | `mainnet` |
| `--datadir` | where the chain and the node's wallet are kept | the platform's data directory, `sidepot` |
| `--mainchain-grpc-url` | the enforcer's gRPC address | |
| `--l1-rpc-url` | the eCash node's JSON-RPC address | |
| `--l1-rpc-cookie` / `--l1-rpc-userpass` | how to log in to it: its cookie file, or `user:password` | |
| `--game-start-height` | the game's first eCash block | 984960 |
| `--game-anchor` | hash of the eCash block that mined the slot's proposal | mainnet block 982,723 |
| `--producer-key` | the game account (64 hex characters) that earns the block bonus of blocks this node produces | none |
| `--rpc-addr`, `--private-rpc-addr` | where the node's own JSON-RPC listens | `127.0.0.1:6088` |
| `--net-addr` | where it listens for other Sidepot nodes | `0.0.0.0:4088` |

The first start reads every eCash block header from the enforcer, which takes about a minute and about 900 MB of
disk.

```bash
sidepot-node-cli game-state                 # the game: final, and as of the latest eCash block
sidepot-node-cli game-account <64 hex>      # one game account
sidepot-node-cli mine --fee-sats 10000      # bid for the next Sidepot block (waits for the next eCash block)
```

`sidepot-node-cli --help` lists the rest. A node on other ports is reached with `--rpc-url`.

## Building

Needs the Rust toolchain named in `rust-toolchain.toml` (rustup installs it).

```bash
git clone --recurse-submodules https://github.com/Coinelius/sidepot-node
cd sidepot-node
cargo build --release -p thunder_app -p thunder_app_cli
```

The programs are `target/release/sidepot-node` and `target/release/sidepot-node-cli`.

## What differs from Thunder

- Slot 88, and its own network magic (`SPOT`), so Sidepot and Thunder nodes never exchange messages.
- A block header commits to the game's state after the block, and names its producer's game account.
- A deposit addressed to `spot:<game account key>` credits that game account instead of making a coin.
- A block's coinbase must pay exactly the cashouts of the eCash blocks it processes.
