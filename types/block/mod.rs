use borsh::BorshSerialize;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{
    hashes::{self, BlockHash, CoinbaseTxid, MerkleRoot, UtreexoNodeHash},
    schema, util,
};

pub mod body;
pub use body::Body;
pub mod coinbase;
pub use coinbase::Coinbase;

#[derive(
    BorshSerialize,
    Clone,
    Debug,
    Deserialize,
    Eq,
    Hash,
    PartialEq,
    Serialize,
    ToSchema,
)]
pub struct Header {
    pub merkle_root: MerkleRoot,
    pub prev_side_hash: Option<BlockHash>,
    #[borsh(serialize_with = "util::borsh::serialize::bitcoin_block_hash")]
    #[schema(value_type = schema::BitcoinBlockHash)]
    pub prev_main_hash: bitcoin::BlockHash,
    /// Utreexo roots
    #[borsh(serialize_with = "util::borsh::serialize::utreexo_roots")]
    #[schema(value_type = Vec<schema::UtreexoNodeHash>)]
    pub roots: Vec<UtreexoNodeHash>,
    /// Sidepot: the account key (BIP340 x-only) of this block's producer. A block bonus this block earns is
    /// credited to that game account.
    #[serde(default)]
    #[schema(value_type = Vec<u8>)]
    pub producer: [u8; 32],
    /// Sidepot: hash of the game state after this block has processed its L1 batches. All zero while the game is
    /// off.
    #[serde(default)]
    #[schema(value_type = Vec<u8>)]
    pub game_hash: [u8; 32],
}

impl Header {
    pub fn compute_coinbase_txid(&self) -> CoinbaseTxid {
        let Self {
            merkle_root,
            prev_side_hash,
            prev_main_hash,
            roots: _,
            producer: _,
            game_hash: _,
        } = self;
        Coinbase::compute_txid(
            merkle_root,
            prev_main_hash,
            prev_side_hash.as_ref(),
        )
    }

    pub fn hash(&self) -> BlockHash {
        hashes::hash_with_scratch_buffer(self).into()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Block {
    pub header: Header,
    pub body: Body,
}
