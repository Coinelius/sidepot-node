//! Sidepot: what the game needs from L1 beyond what the enforcer reports.
//!
//! The enforcer gives a sidechain its deposits, withdrawal events and BMM commitments. The game also reads every
//! L1 block's entries (signed instructions in `OP_RETURN` outputs of ordinary transactions), whether the block
//! holds any transaction besides its coinbase (only such blocks tick the game clock), and the fee of the accepted
//! BMM request (the bid that sizes the block bonus). Those come from the L1 node's own RPC, the node the enforcer
//! already runs against.

use std::{path::PathBuf, sync::OnceLock, time::Duration};

use base64::Engine as _;
use bitcoin::hashes::Hash as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sidepot_core::{Bmm, Domain, Entry, Game, Params};
use sneed::RoTxn;

use crate::{
    archive::{self, Archive},
    state::{self, State},
    types::{
        Address, BlockHash, GetValue as _, Output, OutputContent,
        THIS_SIDECHAIN, proto::mainchain::BlockEvent,
    },
};

/// How this node reads the game. Set once at startup; without it the game is off, as in unit tests of the
/// unmodified sidechain logic.
#[derive(Clone, Debug)]
pub struct Config {
    /// The first L1 block of the game: block 1 of round 1, if it holds a transaction.
    pub start_height: u32,
    /// Hash of the L1 block that mined the slot proposal, in header byte order. Entries are signed for it.
    pub anchor: [u8; 32],
    /// JSON-RPC URL of the L1 node.
    pub l1_rpc_url: String,
    /// The L1 node's cookie file, read on every request since it changes when the node restarts.
    pub l1_rpc_cookie: Option<PathBuf>,
    /// `user:password`, used when there is no cookie file.
    pub l1_rpc_userpass: Option<String>,
    /// The account key (BIP340 x-only) this node names in the blocks it produces, to receive block bonuses.
    pub producer: [u8; 32],
}

static CONFIG: OnceLock<Config> = OnceLock::new();

/// Returns the config back if one was already set.
pub fn set_config(config: Config) -> Result<(), Box<Config>> {
    CONFIG.set(config).map_err(Box::new)
}

pub fn config() -> Option<&'static Config> {
    CONFIG.get()
}

impl Config {
    pub fn domain(&self) -> Domain {
        Domain {
            anchor: self.anchor,
            slot: THIS_SIDECHAIN,
        }
    }
}

/// The game before any L1 block has been processed, if the game is on.
pub fn initial() -> Option<Game> {
    config().map(|config| {
        Game::new(Params::default(), config.domain(), config.start_height)
    })
}

/// What the game reads from one L1 block.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct MainGameData {
    /// Transactions in the block, coinbase included. A block with only its coinbase does not tick the clock.
    pub n_tx: u32,
    /// Every well-formed entry in the block's non-coinbase transactions, encoded, in block order.
    pub entries: Vec<Vec<u8>>,
    /// Fee, in sats, of the BMM request (M8) this block accepted for this slot, if it accepted one.
    pub bmm_bid: Option<u64>,
}

impl MainGameData {
    pub fn is_empty_block(&self) -> bool {
        self.n_tx <= 1
    }

    pub fn decode_entries(&self) -> Vec<Entry> {
        self.entries
            .iter()
            .filter_map(|bytes| Entry::decode(bytes).ok())
            .collect()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("the game is not configured")]
    NoConfig,
    #[error("cannot read the L1 node's cookie file {path}: {source}")]
    Cookie {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("L1 node RPC `{method}` failed: {message}")]
    Rpc {
        method: &'static str,
        message: String,
    },
    #[error("L1 node returned a malformed block {block_hash}: {what}")]
    Malformed {
        block_hash: bitcoin::BlockHash,
        what: &'static str,
    },
}

fn rpc(
    config: &Config,
    method: &'static str,
    params: Value,
) -> Result<Value, FetchError> {
    let auth = match (&config.l1_rpc_cookie, &config.l1_rpc_userpass) {
        (Some(path), _) => std::fs::read_to_string(path).map_err(|source| {
            FetchError::Cookie {
                path: path.clone(),
                source,
            }
        })?,
        (None, Some(userpass)) => userpass.clone(),
        (None, None) => String::new(),
    };
    let auth = base64::engine::general_purpose::STANDARD.encode(auth.trim());
    let err = |message: String| FetchError::Rpc { method, message };
    let reply: Value = ureq::post(&config.l1_rpc_url)
        .timeout(Duration::from_secs(120))
        .set("Authorization", &format!("Basic {auth}"))
        .send_json(
            json!({"jsonrpc": "2.0", "id": 0, "method": method, "params": params}),
        )
        .map_err(|e| err(e.to_string()))?
        .into_json()
        .map_err(|e| err(e.to_string()))?;
    match &reply["error"] {
        Value::Null => Ok(reply["result"].clone()),
        e => Err(err(e.to_string())),
    }
}

fn script(output: &Value) -> Option<Vec<u8>> {
    const_hex::decode(output["scriptPubKey"]["hex"].as_str()?).ok()
}

fn sats(value: &Value) -> Option<u64> {
    Some((value.as_f64()? * 1e8).round() as u64)
}

/// The bid of the block's BMM for this slot: the fee of the M8 transaction that commits to the h* the coinbase's
/// M7 accepts and to the previous block (an M8 is only valid in the block after the one it names).
fn bmm_bid(block: &Value, txs: &[Value]) -> Option<u64> {
    let mut prev =
        const_hex::decode(block["previousblockhash"].as_str()?).ok()?;
    prev.reverse();
    let hstar =
        txs.first()?["vout"].as_array()?.iter().find_map(
            |o| match script(o)?.as_slice() {
                [0x6a, 0x25, 0xd1, 0x61, 0x73, 0x68, slot, h @ ..]
                    if *slot == THIS_SIDECHAIN && h.len() == 32 =>
                {
                    Some(h.to_vec())
                }
                _ => None,
            },
        )?;
    txs.iter().skip(1).find_map(|tx| {
        let commits =
            tx["vout"]
                .as_array()?
                .iter()
                .any(|o| match script(o).as_deref() {
                    Some([0x6a, 0x44, 0x00, 0xbf, 0x00, slot, rest @ ..]) => {
                        *slot == THIS_SIDECHAIN
                            && rest.len() == 64
                            && rest[..32] == hstar[..]
                            && rest[32..] == prev[..]
                    }
                    _ => false,
                });
        commits.then(|| sats(&tx["fee"])).flatten()
    })
}

/// Reads one L1 block from the L1 node. Needs the block's undo data (an unpruned node), for transaction fees.
pub fn fetch(
    block_hash: &bitcoin::BlockHash,
) -> Result<MainGameData, FetchError> {
    let config = config().ok_or(FetchError::NoConfig)?;
    let block = rpc(config, "getblock", json!([block_hash.to_string(), 3]))?;
    let txs = block["tx"].as_array().ok_or(FetchError::Malformed {
        block_hash: *block_hash,
        what: "no transaction list",
    })?;
    let entries = txs
        .iter()
        .skip(1)
        .flat_map(|tx| tx["vout"].as_array().into_iter().flatten())
        .filter_map(script)
        .filter_map(|script| Entry::from_script(&script)?.ok())
        .map(|entry| entry.encode())
        .collect();
    Ok(MainGameData {
        n_tx: txs.len() as u32,
        entries,
        bmm_bid: bmm_bid(&block, txs),
    })
}

/// The game after a Sidepot block has processed its L1 batches, and what that block owes.
#[derive(Clone, Debug)]
pub struct Advance {
    pub game: Game,
    /// The cashouts of the processed batches, in order: the block's coinbase must end with exactly these.
    pub payouts: Vec<Output>,
}

impl Advance {
    pub fn game_hash(&self) -> [u8; 32] {
        game_hash(&self.game)
    }
}

fn credit_deposits(game: &mut Game, events: &[BlockEvent]) {
    for event in events {
        if let BlockEvent::Deposit(deposit) = event
            && let Some(player) = deposit.game_account
        {
            game.deposit(player, deposit.output.get_value().to_sat());
        }
    }
}

/// The hash a block header commits to for a game state.
pub fn game_hash(game: &Game) -> [u8; 32] {
    let bytes =
        bincode::serialize(game).expect("a game state always serializes");
    blake3::hash(&bytes).into()
}

#[derive(Debug, thiserror::Error)]
pub enum AdvanceError {
    #[error(transparent)]
    Archive(#[from] archive::Error),
    #[error(transparent)]
    State(#[from] state::Error),
}

/// Runs the game for a Sidepot block built on `prev_side_hash` whose BMM request names the L1 block
/// `prev_main_hash` (so it is accepted, if at all, by an L1 block at that height plus one, H).
///
/// Forced inclusion: the block must process every L1 block from the one after the last its parent processed up
/// to H - 2, in height order. For each: deposits into game accounts are credited, then the block's batch of
/// entries is processed (or the block is passed over, if it holds only a coinbase), then its BMM result settles
/// the block bonus. The bonus goes to the producer named in the header of the Sidepot block that L1 block
/// accepted, if that block is an ancestor of this one; a commitment to an unknown or abandoned block earns
/// nothing.
///
/// Returns `None` while the game is off.
pub fn advance(
    rotxn: &RoTxn,
    archive: &Archive,
    state: &State,
    prev_main_hash: bitcoin::BlockHash,
    prev_side_hash: Option<BlockHash>,
) -> Result<Option<Advance>, AdvanceError> {
    let Some(config) = config() else {
        return Ok(None);
    };
    // The game the parent block left. A block is not always built on the tip, so look the parent up.
    let parent_game = match prev_side_hash {
        Some(parent) => {
            let parent_height = archive.get_height(rotxn, parent)?;
            state.try_get_game_at(rotxn, parent_height)?
        }
        None => None,
    };
    let game = parent_game.unwrap_or_else(|| {
        Game::new(Params::default(), config.domain(), config.start_height)
    });
    // H - 2 is the parent of the block the BMM request names.
    let named = archive.get_main_header_info(rotxn, &prev_main_hash)?;
    if named.height == 0 {
        return Ok(Some(Advance {
            game,
            payouts: Vec::new(),
        }));
    }
    let last = (named.prev_block_hash, named.height - 1);
    run(rotxn, archive, config, game, last, prev_side_hash).map(Some)
}

/// The game as it will stand once every L1 block up to `main_tip` has been processed: the tip's game run on
/// over the L1 blocks no Sidepot block has processed yet. Not consensus state: an L1 reorg can still change it,
/// and a block bonus it shows goes only to blocks that stay in the chain. For showing a move as soon as its L1
/// block is mined, before the two blocks it takes to become part of a Sidepot block.
///
/// Returns `None` while the game is off.
pub fn preview(
    rotxn: &RoTxn,
    archive: &Archive,
    state: &State,
    main_tip: bitcoin::BlockHash,
) -> Result<Option<Advance>, AdvanceError> {
    let Some(config) = config() else {
        return Ok(None);
    };
    let game = state.try_get_game(rotxn)?.unwrap_or_else(|| {
        Game::new(Params::default(), config.domain(), config.start_height)
    });
    let tip = archive.get_main_header_info(rotxn, &main_tip)?;
    let side_tip = state.try_get_tip(rotxn)?;
    run(
        rotxn,
        archive,
        config,
        game,
        (main_tip, tip.height),
        side_tip,
    )
    .map(Some)
}

/// Runs `game` over the L1 blocks after the last it processed, up to and including `last` (a block hash and its
/// height), for a block whose parent is `prev_side_hash`.
/// How many L1 blocks hold something for the next Sidepot block to settle: blocks the tip's game has not
/// processed, at most one below `main_tip` (the newest a block bid for now could process), that carry entries
/// or deposits into game accounts, or that open the game. A producer that bids only when this is above zero
/// pays for no empty Sidepot blocks.
///
/// Returns `None` while the game is off.
pub fn to_settle(
    rotxn: &RoTxn,
    archive: &Archive,
    state: &State,
    main_tip: bitcoin::BlockHash,
) -> Result<Option<u32>, AdvanceError> {
    let Some(config) = config() else {
        return Ok(None);
    };
    let processed = state
        .try_get_game(rotxn)?
        .map_or(config.start_height.saturating_sub(1), |game| game.height);
    let tip = archive.get_main_header_info(rotxn, &main_tip)?;
    let (mut hash, mut height) =
        (tip.prev_block_hash, tip.height.saturating_sub(1));
    let mut count = 0;
    while height > processed && hash != bitcoin::BlockHash::all_zeros() {
        let data = archive.get_main_game_data(rotxn, &hash)?;
        let info = archive.get_main_block_info(rotxn, &hash)?;
        let deposit = info.events.iter().any(|event| {
            matches!(event, BlockEvent::Deposit(deposit) if deposit.game_account.is_some())
        });
        if deposit
            || !data.decode_entries().is_empty()
            || height == config.start_height
        {
            count += 1;
        }
        hash = archive.get_main_header_info(rotxn, &hash)?.prev_block_hash;
        height -= 1;
    }
    Ok(Some(count))
}

fn run(
    rotxn: &RoTxn,
    archive: &Archive,
    config: &Config,
    mut game: Game,
    last: (bitcoin::BlockHash, u32),
    prev_side_hash: Option<BlockHash>,
) -> Result<Advance, AdvanceError> {
    let mut payouts = Vec::new();
    // L1 blocks to process, newest first.
    let mut pending = Vec::new();
    let (mut hash, mut height) = last;
    while height > game.height {
        pending.push((hash, height));
        hash = archive.get_main_header_info(rotxn, &hash)?.prev_block_hash;
        if height == 0 {
            break;
        }
        height -= 1;
    }
    for (hash, height) in pending.into_iter().rev() {
        let data = archive.get_main_game_data(rotxn, &hash)?;
        let info = archive.get_main_block_info(rotxn, &hash)?;
        if height == config.start_height {
            // Deposits into game accounts made before the game began (back to the block that mined the slot
            // proposal, before which the slot could hold nothing) are credited as it begins.
            let mut early = archive.get_main_header_info(rotxn, &hash)?;
            while early.prev_block_hash != bitcoin::BlockHash::all_zeros()
                && early.block_hash.to_byte_array() != config.anchor
            {
                let early_hash = early.prev_block_hash;
                let early_info =
                    archive.get_main_block_info(rotxn, &early_hash)?;
                credit_deposits(&mut game, &early_info.events);
                early = archive.get_main_header_info(rotxn, &early_hash)?;
            }
        }
        credit_deposits(&mut game, &info.events);
        if data.is_empty_block() {
            game.process_empty(height);
        } else {
            let report = game.process_batch(height, &data.decode_entries());
            payouts.extend(report.payouts.into_iter().map(|(dest, sats)| {
                Output {
                    address: Address(dest),
                    content: OutputContent::Value(bitcoin::Amount::from_sat(
                        sats,
                    )),
                }
            }));
        }
        let producer = match (info.bmm_commitment, prev_side_hash) {
            (Some(accepted), Some(parent)) => {
                match archive.try_get_header(rotxn, accepted)? {
                    Some(header)
                        if archive
                            .is_descendant(rotxn, accepted, parent)? =>
                    {
                        Some(header.producer)
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        let bmm = producer
            .zip(data.bmm_bid)
            .map(|(producer, bid)| Bmm { producer, bid });
        game.on_bmm(bmm);
    }
    Ok(Advance { game, payouts })
}
