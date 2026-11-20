//! Sidepot: playing from this node. The node's wallet seed gives it a game account; a move is a signed entry
//! that the enforcer's wallet posts in an `OP_RETURN` output on L1, and a deposit to `spot:<key>` funds the
//! account. The round, nonce and expiry of a move come from the node's own view of the game at the L1 tip.

use serde::Serialize;
use sidepot_core::{Entry, Game};
use thunder::types::Address;

use crate::app::{App, Error};

/// A buy stays good for this many counted L1 blocks, and its default price limit leaves room for as many
/// price steps.
pub const BUY_WINDOW: u32 = 3;
/// A cashout is not time-critical: it stays good for a day of counted blocks.
pub const CASHOUT_WINDOW: u32 = 144;
/// L1 fee of a move's transaction when none is given.
pub const DEFAULT_ENTRY_FEE_SATS: u64 = 1_000;

/// Sidechain fee of a `game_fund` transaction when none is given.
pub const DEFAULT_FUND_FEE_SATS: u64 = 1_000;

/// The price one step up: 1% per L1 block with a buy.
pub fn step_price(price: u64) -> u64 {
    let stepped = u128::from(price) * 101 / 100;
    u64::try_from(stepped).unwrap_or(u64::MAX)
}

/// The most a buy pays per key by default: the price after `BUY_WINDOW` steps.
pub fn default_max_price(price: u64) -> u64 {
    (0..BUY_WINDOW).fold(price, |price, _| step_price(price))
}

/// One account as the game holds it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct AccountView {
    pub balance_sats: u64,
    pub keys: u64,
    pub nonce: u64,
}

impl AccountView {
    pub fn of(game: &Game, key: &[u8; 32]) -> Option<Self> {
        game.accounts.get(key).map(|account| Self {
            balance_sats: game.balance(key),
            keys: account.keys,
            nonce: account.nonce,
        })
    }
}

/// The game and this node's account in it, final and as of the L1 tip.
#[derive(Clone, Debug)]
pub struct GameView {
    /// The game after the last Sidepot block, or the opening game before the first.
    pub confirmed: Game,
    /// The game run on to the L1 tip.
    pub preview: Game,
    /// Height of the L1 tip.
    pub tip_height: u32,
    /// The game's first L1 block.
    pub start_height: u32,
    /// This node's account key, if its wallet has a seed.
    pub key: Option<[u8; 32]>,
    /// L1 blocks holding moves or deposits that the next Sidepot block would settle.
    pub to_settle: u32,
    /// L1 blocks holding ordinary deposits or withdrawal events that the next Sidepot block would connect.
    pub peg_waiting: u32,
}

impl GameView {
    /// False until the next L1 block can be the game's first.
    pub fn open(&self) -> bool {
        self.tip_height + 1 >= self.start_height
    }

    /// Counted blocks left on the clock.
    pub fn blocks_left(&self) -> u32 {
        if self.tip_height < self.start_height {
            self.preview.params.start_time
        } else {
            self.preview.deadline.saturating_sub(self.preview.clock)
        }
    }

    pub fn account(&self) -> Option<AccountView> {
        self.key
            .as_ref()
            .and_then(|key| AccountView::of(&self.preview, key))
    }

    pub fn confirmed_account(&self) -> Option<AccountView> {
        self.key
            .as_ref()
            .and_then(|key| AccountView::of(&self.confirmed, key))
    }
}

/// A move this node posted, or signed for something else to post.
#[derive(Clone, Debug, Serialize)]
pub struct Move {
    /// The L1 transaction that carries the entry, if this node posted it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub txid: Option<bitcoin::Txid>,
    /// The signed entry in hex, if it was only signed: the data of one `OP_RETURN` output (a single push) of
    /// an L1 transaction that whoever asked must send.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
    pub round: u32,
    pub nonce: u64,
    /// Last tick of the game clock at which the entry counts.
    pub expires: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keys: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_price_sats: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount_sats: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dest: Option<Address>,
}

fn game_err(msg: impl Into<String>) -> Error {
    Error::Game(msg.into())
}

impl App {
    /// The game as this node sees it now. Asks the enforcer for the L1 tip and, if the node has not stored
    /// that block yet, fetches it: a node that neither produces blocks nor has peers learns of new L1 blocks
    /// only here.
    pub async fn game_view(&self) -> Result<GameView, Error> {
        let config = thunder::game::config()
            .ok_or_else(|| game_err("the game is not configured"))?;
        let tip = self
            .node
            .with_cusf_mainchain(|cusf_mainchain| cusf_mainchain.clone())
            .get_chain_tip()
            .await?;
        if !self
            .node
            .request_mainchain_ancestor_infos(tip.block_hash)
            .await?
        {
            return Err(game_err("the L1 tip is not available yet"));
        }
        let preview = self
            .node
            .preview_game(tip.block_hash)?
            .ok_or_else(|| game_err("the game is not configured"))?;
        let confirmed = match self.node.try_get_game()? {
            Some(game) => game,
            None => thunder::game::initial()
                .ok_or_else(|| game_err("the game is not configured"))?,
        };
        let to_settle =
            self.node.game_to_settle(tip.block_hash)?.unwrap_or(0);
        let peg_waiting = self.node.peg_waiting(tip.block_hash)?;
        Ok(GameView {
            to_settle,
            peg_waiting,
            confirmed,
            preview,
            tip_height: tip.height,
            start_height: config.start_height,
            key: self.wallet.game_account_key().ok(),
        })
    }

    /// The next nonce for this node's account: above the account's last accepted one and above any this node
    /// has already used.
    fn next_game_nonce(&self, account: Option<AccountView>) -> u64 {
        let mut last_used = self.game_nonce.lock();
        let nonce =
            account.map_or(0, |account| account.nonce).max(*last_used) + 1;
        *last_used = nonce;
        nonce
    }

    /// Posts the entry with the enforcer's wallet, or hands its bytes back in hex.
    async fn post_or_keep(
        &self,
        entry: &Entry,
        post: Option<Option<u64>>,
    ) -> Result<(Option<bitcoin::Txid>, Option<String>), Error> {
        match post {
            Some(fee_sats) => {
                Ok((Some(self.post_entry(entry, fee_sats).await?), None))
            }
            None => Ok((None, Some(const_hex::encode(entry.encode())))),
        }
    }

    async fn post_entry(
        &self,
        entry: &Entry,
        fee_sats: Option<u64>,
    ) -> Result<bitcoin::Txid, Error> {
        let mut wallet = self
            .game_wallet
            .clone()
            .ok_or(Error::NoCusfMainchainWalletClient)?;
        let txid = wallet
            .send_op_return(
                &entry.encode(),
                Some(fee_sats.unwrap_or(DEFAULT_ENTRY_FEE_SATS)),
            )
            .await?;
        Ok(txid)
    }

    /// Buys `keys` keys in the current round for at most `max_price_sats` each (unset: the price after three
    /// steps). The entry lapses after three counted L1 blocks.
    pub async fn game_buy(
        &self,
        keys: u16,
        max_price_sats: Option<u64>,
        fee_sats: Option<u64>,
    ) -> Result<Move, Error> {
        self.buy(keys, max_price_sats, Some(fee_sats)).await
    }

    /// Signs the same buy without posting it, for a wallet elsewhere to carry (a node on a remote enforcer has
    /// no L1 wallet of its own).
    pub async fn game_sign_buy(
        &self,
        keys: u16,
        max_price_sats: Option<u64>,
    ) -> Result<Move, Error> {
        self.buy(keys, max_price_sats, None).await
    }

    /// `post` is the L1 fee to post the entry with (`Some(None)`: the default fee), or `None` to only sign.
    async fn buy(
        &self,
        keys: u16,
        max_price_sats: Option<u64>,
        post: Option<Option<u64>>,
    ) -> Result<Move, Error> {
        if !(1..=1000).contains(&keys) {
            return Err(game_err("a buy is for 1 to 1000 keys"));
        }
        if post.is_some() && self.game_wallet.is_none() {
            return Err(Error::NoCusfMainchainWalletClient);
        }
        let keypair = self.wallet.game_keypair()?;
        let view = self.game_view().await?;
        if !view.open() {
            return Err(game_err(format!(
                "the game opens at L1 block {}; the tip is {}",
                view.start_height, view.tip_height
            )));
        }
        let game = &view.preview;
        let max_price =
            max_price_sats.unwrap_or_else(|| default_max_price(game.price));
        if max_price < game.price {
            return Err(game_err(format!(
                "the price is {} sats per key, above the limit of {max_price}",
                game.price
            )));
        }
        let account = view.account();
        let cost = u64::from(keys).saturating_mul(game.price);
        let balance = account.map_or(0, |account| account.balance_sats);
        if balance < cost {
            return Err(game_err(format!(
                "the game account holds {balance} sats; {keys} keys cost {cost} at the current price"
            )));
        }
        let domain = thunder::game::config()
            .ok_or_else(|| game_err("the game is not configured"))?
            .domain();
        let nonce = self.next_game_nonce(account);
        let expires = game.clock + BUY_WINDOW;
        let entry = Entry::buy(
            &keypair, &domain, game.round, keys, max_price, nonce, expires,
        );
        let (txid, signed) = self.post_or_keep(&entry, post).await?;
        Ok(Move {
            txid,
            entry: signed,
            round: game.round,
            nonce,
            expires,
            keys: Some(keys),
            max_price_sats: Some(max_price),
            amount_sats: None,
            dest: None,
        })
    }

    /// Pays `amount_sats` (0: the whole balance) out of the game account to `dest` as an ordinary Sidepot
    /// coin. Unset, `dest` is a new address of this node's wallet.
    pub async fn game_cashout(
        &self,
        amount_sats: u64,
        dest: Option<Address>,
        fee_sats: Option<u64>,
    ) -> Result<Move, Error> {
        self.cashout(amount_sats, dest, Some(fee_sats)).await
    }

    /// Signs the same cashout without posting it.
    pub async fn game_sign_cashout(
        &self,
        amount_sats: u64,
        dest: Option<Address>,
    ) -> Result<Move, Error> {
        self.cashout(amount_sats, dest, None).await
    }

    async fn cashout(
        &self,
        amount_sats: u64,
        dest: Option<Address>,
        post: Option<Option<u64>>,
    ) -> Result<Move, Error> {
        if post.is_some() && self.game_wallet.is_none() {
            return Err(Error::NoCusfMainchainWalletClient);
        }
        let keypair = self.wallet.game_keypair()?;
        let view = self.game_view().await?;
        let account = view.account();
        let balance = account.map_or(0, |account| account.balance_sats);
        if balance == 0 || balance < amount_sats {
            return Err(game_err(format!(
                "the game account holds {balance} sats"
            )));
        }
        let dest = match dest {
            Some(dest) => dest,
            None => self.wallet.get_new_address()?,
        };
        let domain = thunder::game::config()
            .ok_or_else(|| game_err("the game is not configured"))?
            .domain();
        let nonce = self.next_game_nonce(account);
        let expires = view.preview.clock + CASHOUT_WINDOW;
        let entry = Entry::cashout(
            &keypair,
            &domain,
            amount_sats,
            dest.0,
            nonce,
            expires,
        );
        let (txid, signed) = self.post_or_keep(&entry, post).await?;
        Ok(Move {
            txid,
            entry: signed,
            round: view.preview.round,
            nonce,
            expires,
            keys: None,
            max_price_sats: None,
            amount_sats: Some(amount_sats),
            dest: Some(dest),
        })
    }

    /// Moves `amount_sats` of this node's wallet coins into the game account `account` (unset: this node's
    /// own) with a sidechain transaction that holds a game deposit output. The account is credited by the
    /// Sidepot block that includes the transaction. Returns the transaction id.
    pub fn game_fund(
        &self,
        amount_sats: u64,
        account: Option<[u8; 32]>,
        fee_sats: Option<u64>,
    ) -> Result<thunder::types::Txid, Error> {
        if amount_sats == 0 {
            return Err(game_err("the amount must be above zero"));
        }
        let account = match account {
            Some(account) => account,
            None => self.wallet.game_account_key()?,
        };
        let l1_form = format!("spot:{}", const_hex::encode(account));
        if sidepot_core::deposit_player(&l1_form) != Some(account) {
            return Err(game_err("not a valid account key"));
        }
        let accumulator = self.node.get_tip_accumulator()?;
        let tx = self.wallet.create_game_fund(
            &accumulator,
            account,
            bitcoin::Amount::from_sat(amount_sats),
            bitcoin::Amount::from_sat(
                fee_sats.unwrap_or(DEFAULT_FUND_FEE_SATS),
            ),
        )?;
        let txid = tx.txid();
        let () = self.sign_and_send(tx)?;
        Ok(txid)
    }

    /// Deposits `value_sats` from the enforcer's wallet into this node's game account.
    pub async fn game_deposit(
        &self,
        value_sats: u64,
        fee_sats: u64,
    ) -> Result<bitcoin::Txid, Error> {
        let key = self.wallet.game_account_key()?;
        let mut wallet = self
            .game_wallet
            .clone()
            .ok_or(Error::NoCusfMainchainWalletClient)?;
        let address = format!("spot:{}", const_hex::encode(key));
        let txid = wallet
            .create_deposit_tx_to(address, value_sats, fee_sats)
            .await?;
        Ok(txid)
    }
}
