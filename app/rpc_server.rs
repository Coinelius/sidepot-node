use std::{collections::HashSet, net::SocketAddr};

use bitcoin::Amount;
use jsonrpsee::{
    core::{RpcResult, async_trait, middleware::RpcServiceBuilder},
    server::Server,
    types::ErrorObject,
};
use thunder::types::{
    Address, Block, M6id, PointedOutput, SpentOutput, Txid, WithdrawalBundle,
    net::{Peer, PeerAddress},
    wallet::{Balance, TransferDests},
};
use thunder_app_rpc_api::{self as rpc_api, typewit::const_marker::Bool};
use tower_http::{
    cors::CorsLayer,
    request_id::{
        MakeRequestId, PropagateRequestIdLayer, RequestId, SetRequestIdLayer,
    },
    trace::{DefaultOnFailure, DefaultOnResponse, TraceLayer},
};

use crate::app::App;

fn custom_err_msg(err_msg: impl Into<String>) -> ErrorObject<'static> {
    ErrorObject::owned(-1, err_msg.into(), Option::<()>::None)
}

fn custom_err<Error>(error: Error) -> ErrorObject<'static>
where
    anyhow::Error: From<Error>,
{
    let error = anyhow::Error::from(error);
    custom_err_msg(format!("{error:#}"))
}

/// Sidepot: a game state as the RPC reports it.
fn game_json(game: &sidepot_core::Game) -> serde_json::Value {
    serde_json::json!({
        "l1_height": game.height,
        "clock": game.clock,
        "round": game.round,
        "deadline": game.deadline,
        "price_sats": game.price,
        "pot_sats": game.pot,
        "bonus_pool_sats": game.streak_pool,
        "streak": game.streak().map(|(producer, blocks)| serde_json::json!({
            "producer": const_hex::encode(producer),
            "blocks": blocks,
        })),
        "accounts": game.accounts.len(),
        // Keys bought this round, by everyone: an account's keys are those it holds in the current round.
        "keys": game.accounts.values().map(|account| account.keys).sum::<u64>(),
    })
}

/// Sidepot: one game account as the RPC reports it, or null if the game has none for the key.
fn account_json(
    game: &sidepot_core::Game,
    key: &[u8; 32],
) -> serde_json::Value {
    match game.accounts.get(key) {
        Some(account) => serde_json::json!({
            "balance_sats": game.balance(key),
            "keys": account.keys,
            "nonce": account.nonce,
        }),
        None => serde_json::Value::Null,
    }
}

/// Sidepot: the game run on to the L1 tip the enforcer reports, for showing moves before a Sidepot block has
/// processed them. `None` if it cannot be worked out just now (the tip's data has not been stored yet).
async fn game_preview(app: &App) -> Option<sidepot_core::Game> {
    app.game_view().await.ok().map(|view| view.preview)
}

#[derive(Clone)]
#[repr(transparent)]
pub struct RpcServerImpl<const ENABLE_PRIVATE_API: bool> {
    app: App,
}

pub struct PrivateOnlyRpcServerImpl;

#[async_trait]
impl rpc_api::open_api::RpcServer for PrivateOnlyRpcServerImpl {
    async fn openapi_schema(&self) -> RpcResult<utoipa::openapi::OpenApi> {
        use utoipa::OpenApi as _;
        let mut res = rpc_api::open_api::RpcDoc::openapi();
        res.merge(rpc_api::node::PrivateRpcDoc::openapi());
        res.merge(rpc_api::wallet::RpcDoc::openapi());
        Ok(res)
    }
}

#[async_trait]
impl rpc_api::open_api::RpcServer for RpcServerImpl<false> {
    async fn openapi_schema(&self) -> RpcResult<utoipa::openapi::OpenApi> {
        use utoipa::OpenApi as _;
        let mut res = rpc_api::open_api::RpcDoc::openapi();
        res.merge(rpc_api::node::RpcDoc::openapi());
        Ok(res)
    }
}

#[async_trait]
impl rpc_api::open_api::RpcServer for RpcServerImpl<true> {
    async fn openapi_schema(&self) -> RpcResult<utoipa::openapi::OpenApi> {
        use utoipa::OpenApi as _;
        let mut res = rpc_api::open_api::RpcDoc::openapi();
        res.merge(rpc_api::node::PrivateRpcDoc::openapi());
        res.merge(rpc_api::node::RpcDoc::openapi());
        res.merge(rpc_api::wallet::RpcDoc::openapi());
        Ok(res)
    }
}

#[async_trait]
impl rpc_api::node::PrivateRpcServer for RpcServerImpl<true> {
    async fn connect_peer(&self, addr: PeerAddress) -> RpcResult<()> {
        let resolved_addr = thunder::net::resolve_peer_address(
            self.app.node.dns_resolver(),
            addr,
        )
        .await
        .map_err(custom_err)?;
        self.app
            .node
            .connect_peer(resolved_addr)
            .map_err(custom_err)
    }

    async fn forget_peer(&self, addr: PeerAddress) -> RpcResult<()> {
        match self.app.node.forget_peer(&addr) {
            Ok(_) => Ok(()),
            Err(err) => Err(custom_err(err)),
        }
    }

    async fn invalidate_block(
        &self,
        block_hash: thunder::types::BlockHash,
    ) -> RpcResult<()> {
        self.app
            .node
            .invalidate_block(block_hash)
            .map_err(custom_err)
    }

    async fn remove_from_mempool(&self, txid: Txid) -> RpcResult<()> {
        self.app.node.remove_from_mempool(txid).map_err(custom_err)
    }

    async fn stop(&self) {
        std::process::exit(0);
    }
}

#[async_trait]
impl<const ENABLE_PRIVATE_API: bool>
    rpc_api::node::get_block::RpcServer<Bool<false>>
    for RpcServerImpl<ENABLE_PRIVATE_API>
{
    async fn get_block(
        &self,
        block_hash: thunder::types::BlockHash,
        _verbose: Bool<false>,
    ) -> RpcResult<
        Option<<Bool<false> as rpc_api::node::get_block::Verbosity>::Response>,
    > {
        let Some(header) = self
            .app
            .node
            .try_get_header(block_hash)
            .map_err(custom_err)?
        else {
            return Ok(None);
        };
        let body = self.app.node.get_body(block_hash).map_err(custom_err)?;
        let block = thunder::types::Block { header, body };
        Ok(Some(block))
    }
}

#[async_trait]
impl<const ENABLE_PRIVATE_API: bool>
    rpc_api::node::get_block::RpcServer<Bool<true>>
    for RpcServerImpl<ENABLE_PRIVATE_API>
{
    async fn get_block(
        &self,
        block_hash: thunder::types::BlockHash,
        _verbose: Bool<true>,
    ) -> RpcResult<
        Option<<Bool<true> as rpc_api::node::get_block::Verbosity>::Response>,
    > {
        let Some(header) = self
            .app
            .node
            .try_get_header(block_hash)
            .map_err(custom_err)?
        else {
            return Ok(None);
        };
        let thunder::types::Body {
            coinbase,
            transactions,
            authorizations,
        } = self.app.node.get_body(block_hash).map_err(custom_err)?;
        let transactions_verbose = transactions
            .into_iter()
            .map(|tx| {
                Ok(thunder_app_rpc_api::node::TransactionVerbose {
                    canonical_bytes: tx.canonical_bytes()?,
                    tx,
                })
            })
            .collect::<std::io::Result<_>>()
            .map_err(custom_err)?;
        let body = thunder_app_rpc_api::node::get_block::BodyVerbose {
            coinbase,
            transactions: transactions_verbose,
            authorizations,
        };
        let block_verbose =
            thunder_app_rpc_api::node::get_block::BlockVerbose { header, body };
        Ok(Some(block_verbose))
    }
}

#[async_trait]
impl<const ENABLE_PRIVATE_API: bool> rpc_api::node::RpcServer
    for RpcServerImpl<ENABLE_PRIVATE_API>
{
    async fn connect_block(
        &self,
        block: Block,
        main_block_hash: bitcoin::BlockHash,
    ) -> RpcResult<bool> {
        self.app
            .local_pool
            .spawn_pinned({
                let app = self.app.clone();
                move || async move {
                    app.connect_block(block, main_block_hash)
                        .await
                        .map_err(custom_err)
                }
            })
            .await
            .unwrap()
    }

    async fn get_block_hash(
        &self,
        height: u32,
    ) -> RpcResult<Option<thunder::types::BlockHash>> {
        self.app.node.try_get_block_hash(height).map_err(custom_err)
    }

    async fn get_block_ids(
        &self,
        block_hash: thunder::types::BlockHash,
    ) -> RpcResult<serde_json::Value> {
        let Some(header) = self
            .app
            .node
            .try_get_header(block_hash)
            .map_err(custom_err)?
        else {
            return Ok(serde_json::Value::Null);
        };
        let body = self.app.node.get_body(block_hash).map_err(custom_err)?;
        let txids: Vec<_> =
            body.transactions.iter().map(|tx| tx.txid()).collect();
        Ok(serde_json::json!({
            "coinbase_txid": header.compute_coinbase_txid(),
            "txids": txids,
        }))
    }

    async fn get_best_sidechain_block_hash(
        &self,
    ) -> RpcResult<Option<thunder::types::BlockHash>> {
        self.app.node.try_get_tip().map_err(custom_err)
    }

    async fn get_best_mainchain_block_hash(
        &self,
    ) -> RpcResult<Option<bitcoin::BlockHash>> {
        let Some(sidechain_hash) =
            self.app.node.try_get_tip().map_err(custom_err)?
        else {
            // No sidechain tip, so no best mainchain block hash.
            return Ok(None);
        };
        let block_hash = self
            .app
            .node
            .get_best_main_verification(sidechain_hash)
            .map_err(custom_err)?;
        Ok(Some(block_hash))
    }

    async fn get_block_index(
        &self,
        block_hash: thunder::types::BlockHash,
    ) -> RpcResult<thunder::types::BlockIndex> {
        let body = self.app.node.get_body(block_hash).map_err(custom_err)?;
        let txs = body
            .transactions
            .iter()
            .map(|tx| {
                Ok(thunder::types::BlockIndexTx {
                    txid: tx.txid(),
                    size: tx.canonical_size()?,
                    raw: const_hex::encode(tx.canonical_bytes()?),
                })
            })
            .collect::<Result<_, std::io::Error>>()
            .map_err(custom_err)?;
        let events = self
            .app
            .node
            .get_block_index_events(block_hash)
            .map_err(custom_err)?;
        Ok(thunder::types::BlockIndex {
            txs,
            deposits: events
                .deposits
                .into_iter()
                .map(|(outpoint, output)| thunder::types::BlockIndexDeposit {
                    outpoint,
                    output,
                })
                .collect(),
            bundle_spends: events
                .bundle_spends
                .into_iter()
                .map(|(outpoint, m6id)| thunder::types::BlockIndexSpend {
                    outpoint,
                    m6id,
                })
                .collect(),
        })
    }

    async fn get_bmm_inclusions(
        &self,
        block_hash: thunder::types::BlockHash,
    ) -> RpcResult<Vec<bitcoin::BlockHash>> {
        self.app
            .node
            .get_bmm_inclusions(block_hash)
            .map_err(custom_err)
    }

    async fn get_stxos(
        &self,
        addresses: HashSet<Address>,
    ) -> RpcResult<Vec<PointedOutput<SpentOutput>>> {
        let res = self
            .app
            .node
            .get_stxos_by_addresses(&addresses)
            .map_err(custom_err)?
            .into_iter()
            .map(|(outpoint, output)| PointedOutput { outpoint, output })
            .collect();
        Ok(res)
    }

    async fn get_transaction(
        &self,
        txid: Txid,
    ) -> RpcResult<Option<rpc_api::node::GetTransactionResponse>> {
        let res =
            self.app
                .node
                .try_get_transaction(txid)
                .map_err(custom_err)?
                .map(|(tx, block_hash)| {
                    rpc_api::node::GetTransactionResponse { tx, block_hash }
                });
        Ok(res)
    }

    async fn get_utxos(
        &self,
        addresses: HashSet<Address>,
    ) -> RpcResult<Vec<PointedOutput>> {
        let res = self
            .app
            .node
            .get_utxos_by_addresses(&addresses)
            .map_err(custom_err)?
            .into_iter()
            .map(|(outpoint, output)| PointedOutput { outpoint, output })
            .collect();
        Ok(res)
    }

    async fn get_withdrawal_bundle(
        &self,
        m6id: M6id,
    ) -> RpcResult<Option<rpc_api::node::GetWithdrawalBundleResponse>> {
        let Some((bundle_info, bundle_status)) = self
            .app
            .node
            .try_get_withdrawal_bundle(&m6id)
            .map_err(custom_err)?
        else {
            return Ok(None);
        };
        let response = rpc_api::node::GetWithdrawalBundleResponse {
            info: bundle_info,
            status: bundle_status,
        };
        Ok(Some(response))
    }

    async fn getblockcount(&self) -> RpcResult<u32> {
        let height = self.app.node.try_get_height().map_err(custom_err)?;
        let block_count = height.map_or(0, |height| height + 1);
        Ok(block_count)
    }

    async fn latest_failed_withdrawal_bundle_height(
        &self,
    ) -> RpcResult<Option<u32>> {
        let height = self
            .app
            .node
            .get_latest_failed_withdrawal_bundle_height()
            .map_err(custom_err)?;
        Ok(height)
    }

    async fn list_peers(&self) -> RpcResult<Vec<Peer>> {
        let peers = self.app.node.get_active_peers();
        Ok(peers)
    }

    async fn list_mempool(&self) -> RpcResult<Vec<thunder::types::MempoolTx>> {
        let txs = self.app.node.get_all_transactions().map_err(custom_err)?;
        let res = txs
            .into_iter()
            .map(|authorized| {
                let tx = authorized.transaction;
                Ok(thunder::types::MempoolTx {
                    txid: tx.txid(),
                    size: tx.canonical_size()?,
                    raw: const_hex::encode(tx.canonical_bytes()?),
                    tx,
                })
            })
            .collect::<Result<_, std::io::Error>>()
            .map_err(custom_err)?;
        Ok(res)
    }

    async fn list_utxos(&self) -> RpcResult<Vec<PointedOutput>> {
        let utxos = self.app.node.get_all_utxos().map_err(custom_err)?;
        let res = utxos
            .into_iter()
            .map(|(outpoint, output)| PointedOutput { outpoint, output })
            .collect();
        Ok(res)
    }

    async fn mainchain_sync_progress(
        &self,
    ) -> RpcResult<thunder::types::MainchainSyncProgress> {
        Ok(self.app.node.mainchain_sync_progress())
    }

    async fn pending_withdrawal_bundle(
        &self,
    ) -> RpcResult<Option<WithdrawalBundle>> {
        self.app
            .node
            .try_get_pending_withdrawal_bundle()
            .map_err(custom_err)
    }

    async fn game_state(&self) -> RpcResult<serde_json::Value> {
        let view = self.app.game_view().await.ok();
        let preview = view.as_ref().map(|view| view.preview.clone());
        // Before the first Sidepot block there is no stored game: report the opening one.
        let Some(game) = self
            .app
            .node
            .try_get_game()
            .map_err(custom_err)?
            .or_else(thunder::game::initial)
        else {
            return Ok(serde_json::Value::Null);
        };
        let mut state = game_json(&game);
        state["start_height"] = thunder::game::config()
            .map_or(serde_json::Value::Null, |config| {
                config.start_height.into()
            });
        // What a block producer watches: L1 blocks with moves or deposits the next Sidepot block would settle.
        state["to_settle"] = view
            .as_ref()
            .map_or(serde_json::Value::Null, |view| view.to_settle.into());
        // Ordinary sidechain business waiting for a block: L1 blocks with deposits that make a coin or with
        // withdrawal events, and transactions in this node's mempool.
        state["peg_waiting"] = view
            .as_ref()
            .map_or(serde_json::Value::Null, |view| view.peg_waiting.into());
        state["mempool_txs"] = self
            .app
            .node
            .get_all_transactions()
            .map_or(serde_json::Value::Null, |txs| txs.len().into());
        state["preview"] =
            preview.as_ref().map_or(serde_json::Value::Null, game_json);
        Ok(state)
    }

    async fn game_account(
        &self,
        account_key: String,
    ) -> RpcResult<serde_json::Value> {
        let key: [u8; 32] = const_hex::decode(&account_key)
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or_else(|| {
                custom_err_msg("an account key is 64 hex characters")
            })?;
        let preview = game_preview(&self.app).await;
        let Some(game) = self
            .app
            .node
            .try_get_game()
            .map_err(custom_err)?
            .or_else(thunder::game::initial)
        else {
            return Ok(serde_json::Value::Null);
        };
        let confirmed = account_json(&game, &key);
        let preview = preview
            .as_ref()
            .map_or(serde_json::Value::Null, |game| account_json(game, &key));
        if confirmed.is_null() && preview.is_null() {
            return Ok(serde_json::Value::Null);
        }
        let mut account = if confirmed.is_null() {
            serde_json::json!({ "balance_sats": 0, "keys": 0, "nonce": 0 })
        } else {
            confirmed
        };
        account["preview"] = preview;
        Ok(account)
    }

    async fn sidechain_wealth_sats(&self) -> RpcResult<u64> {
        let sidechain_wealth =
            self.app.node.get_sidechain_wealth().map_err(custom_err)?;
        Ok(sidechain_wealth.to_sat())
    }

    async fn submit_transaction(
        &self,
        mut transaction: thunder::types::AuthorizedTransaction,
    ) -> RpcResult<Txid> {
        let () = self
            .app
            .submit_transaction(&mut transaction)
            .map_err(custom_err)?;
        Ok(transaction.transaction.txid())
    }
}

#[async_trait]
impl rpc_api::wallet::RpcServer for RpcServerImpl<true> {
    async fn balance(&self) -> RpcResult<Balance> {
        self.app.wallet.get_balance().map_err(custom_err)
    }

    async fn create_deposit(
        &self,
        address: Address,
        value_sats: u64,
        fee_sats: u64,
    ) -> RpcResult<bitcoin::Txid> {
        let app = self.app.clone();
        tokio::task::spawn_blocking(move || {
            app.deposit_blocking(
                address,
                bitcoin::Amount::from_sat(value_sats),
                bitcoin::Amount::from_sat(fee_sats),
            )
            .map_err(custom_err)
        })
        .await
        .unwrap()
    }

    async fn create_transfer(
        &self,
        dest: Address,
        value_sats: u64,
        fee_sats: u64,
    ) -> RpcResult<Txid> {
        let accumulator =
            self.app.node.get_tip_accumulator().map_err(custom_err)?;
        let tx = self
            .app
            .wallet
            .create_transaction(
                &accumulator,
                dest,
                Amount::from_sat(value_sats),
                Amount::from_sat(fee_sats),
            )
            .map_err(custom_err)?;
        let txid = tx.txid();
        let () = self.app.sign_and_send(tx).map_err(custom_err)?;
        Ok(txid)
    }

    async fn create_transfer_many(
        &self,
        dests: TransferDests,
        fee_sats: u64,
    ) -> RpcResult<Txid> {
        let dests = dests
            .0
            .into_iter()
            .map(|(address, value_sats)| {
                (address, Amount::from_sat(value_sats))
            })
            .collect();
        let accumulator =
            self.app.node.get_tip_accumulator().map_err(custom_err)?;
        let tx = self
            .app
            .wallet
            .create_transaction_many(
                &accumulator,
                &dests,
                Amount::from_sat(fee_sats),
            )
            .map_err(custom_err)?;
        let txid = tx.txid();
        let () = self.app.sign_and_send(tx).map_err(custom_err)?;
        Ok(txid)
    }

    async fn create_withdrawal(
        &self,
        mainchain_address: bitcoin::Address<bitcoin::address::NetworkUnchecked>,
        amount_sats: u64,
        fee_sats: u64,
        mainchain_fee_sats: u64,
    ) -> RpcResult<Txid> {
        let accumulator =
            self.app.node.get_tip_accumulator().map_err(custom_err)?;
        let tx = self
            .app
            .wallet
            .create_withdrawal(
                &accumulator,
                mainchain_address,
                Amount::from_sat(amount_sats),
                Amount::from_sat(mainchain_fee_sats),
                Amount::from_sat(fee_sats),
            )
            .map_err(custom_err)?;
        let txid = tx.txid();
        let () = self.app.sign_and_send(tx).map_err(custom_err)?;
        Ok(txid)
    }

    async fn format_deposit_address(
        &self,
        address: Address,
    ) -> RpcResult<String> {
        let deposit_address = address.format_for_deposit();
        Ok(deposit_address)
    }

    async fn generate_mnemonic(&self) -> RpcResult<String> {
        let mnemonic = bip39::Mnemonic::new(
            bip39::MnemonicType::Words12,
            bip39::Language::English,
        );
        Ok(mnemonic.to_string())
    }

    async fn get_block_template(
        &self,
    ) -> RpcResult<rpc_api::wallet::GetBlockTemplateResponse> {
        let template = self
            .app
            .local_pool
            .spawn_pinned({
                let app = self.app.clone();
                move || async move {
                    app.get_block_template().await.map_err(custom_err)
                }
            })
            .await
            .unwrap()?;
        Ok(rpc_api::wallet::GetBlockTemplateResponse {
            critical_hash: template.header.hash(),
            block: Block {
                header: template.header,
                body: template.body,
            },
            fees_sats: template.fees.to_sat(),
        })
    }

    async fn game_my_account(&self) -> RpcResult<serde_json::Value> {
        let app = self.app.clone();
        self.app
            .local_pool
            .spawn_pinned(move || async move {
                let key =
                    app.wallet.game_account_key().map_err(custom_err)?;
                let view = app.game_view().await.map_err(custom_err)?;
                let mut account = serde_json::to_value(
                    view.confirmed_account().unwrap_or_default(),
                )
                .map_err(custom_err)?;
                account["key"] = const_hex::encode(key).into();
                account["address"] =
                    format!("spot:{}", const_hex::encode(key)).into();
                account["preview"] =
                    serde_json::to_value(view.account().unwrap_or_default())
                        .map_err(custom_err)?;
                Ok(account)
            })
            .await
            .unwrap()
    }

    async fn game_buy(
        &self,
        keys: u16,
        max_price_sats: Option<u64>,
        fee_sats: Option<u64>,
    ) -> RpcResult<serde_json::Value> {
        let app = self.app.clone();
        self.app
            .local_pool
            .spawn_pinned(move || async move {
                let sent = app
                    .game_buy(keys, max_price_sats, fee_sats)
                    .await
                    .map_err(custom_err)?;
                serde_json::to_value(sent).map_err(custom_err)
            })
            .await
            .unwrap()
    }

    async fn game_sign_buy(
        &self,
        keys: u16,
        max_price_sats: Option<u64>,
    ) -> RpcResult<serde_json::Value> {
        let app = self.app.clone();
        self.app
            .local_pool
            .spawn_pinned(move || async move {
                let signed = app
                    .game_sign_buy(keys, max_price_sats)
                    .await
                    .map_err(custom_err)?;
                serde_json::to_value(signed).map_err(custom_err)
            })
            .await
            .unwrap()
    }

    async fn game_sign_cashout(
        &self,
        amount_sats: u64,
        dest: Option<Address>,
    ) -> RpcResult<serde_json::Value> {
        let app = self.app.clone();
        self.app
            .local_pool
            .spawn_pinned(move || async move {
                let signed = app
                    .game_sign_cashout(amount_sats, dest)
                    .await
                    .map_err(custom_err)?;
                serde_json::to_value(signed).map_err(custom_err)
            })
            .await
            .unwrap()
    }

    async fn game_cashout(
        &self,
        amount_sats: u64,
        dest: Option<Address>,
        fee_sats: Option<u64>,
    ) -> RpcResult<serde_json::Value> {
        let app = self.app.clone();
        self.app
            .local_pool
            .spawn_pinned(move || async move {
                let sent = app
                    .game_cashout(amount_sats, dest, fee_sats)
                    .await
                    .map_err(custom_err)?;
                serde_json::to_value(sent).map_err(custom_err)
            })
            .await
            .unwrap()
    }

    async fn game_deposit(
        &self,
        value_sats: u64,
        fee_sats: u64,
    ) -> RpcResult<String> {
        let app = self.app.clone();
        self.app
            .local_pool
            .spawn_pinned(move || async move {
                let txid = app
                    .game_deposit(value_sats, fee_sats)
                    .await
                    .map_err(custom_err)?;
                Ok(txid.to_string())
            })
            .await
            .unwrap()
    }

    async fn get_new_address(&self) -> RpcResult<Address> {
        self.app.wallet.get_new_address().map_err(custom_err)
    }

    async fn get_wallet_addresses(&self) -> RpcResult<Vec<Address>> {
        let addrs = self.app.wallet.get_addresses().map_err(custom_err)?;
        let mut res: Vec<_> = addrs.into_iter().collect();
        res.sort_by_key(|addr| addr.as_base58());
        Ok(res)
    }

    async fn get_wallet_utxos(&self) -> RpcResult<Vec<PointedOutput>> {
        let utxos = self.app.wallet.get_utxos().map_err(custom_err)?;
        let utxos = utxos
            .into_iter()
            .map(|(outpoint, output)| PointedOutput { outpoint, output })
            .collect();
        Ok(utxos)
    }

    async fn mine(&self, fee: Option<u64>) -> RpcResult<()> {
        let fee = fee.map(bitcoin::Amount::from_sat);
        self.app
            .local_pool
            .spawn_pinned({
                let app = self.app.clone();
                move || async move { app.mine(fee).await.map_err(custom_err) }
            })
            .await
            .unwrap()
    }

    async fn set_seed_from_mnemonic(&self, mnemonic: String) -> RpcResult<()> {
        let mnemonic =
            bip39::Mnemonic::from_phrase(&mnemonic, bip39::Language::English)
                .map_err(custom_err)?;
        let seed = bip39::Seed::new(&mnemonic, "");
        let seed_bytes: [u8; 64] = seed.as_bytes().try_into().map_err(
            |err: <[u8; 64] as TryFrom<&[u8]>>::Error| custom_err(err),
        )?;
        self.app.wallet.set_seed(&seed_bytes).map_err(custom_err)
    }

    async fn sign_transaction(
        &self,
        transaction: thunder::types::Transaction,
        broadcast: Option<bool>,
    ) -> RpcResult<thunder::types::AuthorizedTransaction> {
        let mut authorized = self
            .app
            .wallet
            .authorize(rand::rng(), transaction)
            .map_err(custom_err)?;
        if let Some(true) = broadcast {
            let () = self
                .app
                .submit_transaction(&mut authorized)
                .map_err(custom_err)?;
        }
        Ok(authorized)
    }
}

#[derive(Clone, Debug)]
struct RequestIdMaker;

impl MakeRequestId for RequestIdMaker {
    fn make_request_id<B>(
        &mut self,
        _: &http::Request<B>,
    ) -> Option<RequestId> {
        use uuid::Uuid;
        // the 'simple' format renders the UUID with no dashes, which
        // makes for easier copy/pasting.
        let id = Uuid::new_v4();
        let id = id.as_simple();
        let id = format!("req_{id}"); // prefix all IDs with "req_", to make them easier to identify

        let Ok(header_value) = http::HeaderValue::from_str(&id) else {
            return None;
        };

        Some(RequestId::new(header_value))
    }
}

pub struct ServerAddesses {
    pub _rpc_addr: SocketAddr,
    pub _private_rpc_addr: SocketAddr,
}

pub async fn run_server(
    app: App,
    private_rpc_addr: SocketAddr,
    rpc_addr: SocketAddr,
) -> anyhow::Result<ServerAddesses> {
    const REQUEST_ID_HEADER: &str = "x-request-id";

    // Ordering here matters! Order here is from official docs on request IDs tracings
    // https://docs.rs/tower-http/latest/tower_http/request_id/index.html#using-trace
    let tracer = || {
        tower::ServiceBuilder::new()
            .layer(SetRequestIdLayer::new(
                http::HeaderName::from_static(REQUEST_ID_HEADER),
                RequestIdMaker,
            ))
            .layer(
                TraceLayer::new_for_http()
                    .make_span_with(move |request: &http::Request<_>| {
                        let request_id = request
                            .headers()
                            .get(http::HeaderName::from_static(
                                REQUEST_ID_HEADER,
                            ))
                            .and_then(|h| h.to_str().ok())
                            .filter(|s| !s.is_empty());

                        tracing::span!(
                            tracing::Level::DEBUG,
                            "request",
                            method = %request.method(),
                            uri = %request.uri(),
                            request_id , // this is needed for the record call below to work
                        )
                    })
                    .on_request(())
                    .on_eos(())
                    .on_response(
                        DefaultOnResponse::new().level(tracing::Level::INFO),
                    )
                    .on_failure(
                        DefaultOnFailure::new().level(tracing::Level::ERROR),
                    ),
            )
            .layer(PropagateRequestIdLayer::new(http::HeaderName::from_static(
                REQUEST_ID_HEADER,
            )))
            .into_inner()
    };

    let http_middleware = || {
        tower::ServiceBuilder::new()
            .layer(tracer())
            .layer(CorsLayer::permissive())
    };
    let rpc_middleware = || RpcServiceBuilder::new().rpc_logger(1024);

    let server = Server::builder()
        .set_http_middleware(http_middleware())
        .set_rpc_middleware(rpc_middleware())
        .build(rpc_addr)
        .await?;
    let rpc_server_addr = server.local_addr()?;

    let (_task_handle, server_addrs) = if private_rpc_addr != rpc_addr {
        let private_rpc_server = Server::builder()
            .set_http_middleware(http_middleware())
            .set_rpc_middleware(rpc_middleware())
            .build(private_rpc_addr)
            .await?;
        let private_rpc_server_addr = private_rpc_server.local_addr()?;

        let rpc_server_handle = {
            let rpc_server_impl = RpcServerImpl::<false> { app: app.clone() };
            let mut rpc_module =
                rpc_api::open_api::RpcServer::into_rpc(rpc_server_impl.clone());
            rpc_module.merge(
                rpc_api::node::get_block::untyped::RpcServer::into_rpc(
                    rpc_server_impl.clone(),
                ),
            )?;
            rpc_module
                .merge(rpc_api::node::RpcServer::into_rpc(rpc_server_impl))?;
            server.start(rpc_module)
        };
        let private_only_rpc_server_handle = {
            let rpc_server_impl = RpcServerImpl::<true> { app };
            let mut rpc_module = rpc_api::open_api::RpcServer::into_rpc(
                PrivateOnlyRpcServerImpl,
            );
            rpc_module.merge(
                rpc_api::node::get_block::untyped::RpcServer::into_rpc(
                    rpc_server_impl.clone(),
                ),
            )?;
            rpc_module.merge(rpc_api::node::PrivateRpcServer::into_rpc(
                rpc_server_impl.clone(),
            ))?;
            rpc_module
                .merge(rpc_api::wallet::RpcServer::into_rpc(rpc_server_impl))?;
            private_rpc_server.start(rpc_module)
        };
        let server_addrs = ServerAddesses {
            _rpc_addr: rpc_server_addr,
            _private_rpc_addr: private_rpc_server_addr,
        };
        let task_handle = tokio::spawn(async {
            tokio::select! {
                () = rpc_server_handle.stopped() => (),
                () = private_only_rpc_server_handle.stopped() => (),
            }
        });
        (task_handle, server_addrs)
    } else {
        let rpc_server_impl = RpcServerImpl::<true> { app };
        let mut rpc_module =
            rpc_api::open_api::RpcServer::into_rpc(rpc_server_impl.clone());
        rpc_module.merge(rpc_api::node::PrivateRpcServer::into_rpc(
            rpc_server_impl.clone(),
        ))?;
        rpc_module.merge(
            rpc_api::node::get_block::untyped::RpcServer::into_rpc(
                rpc_server_impl.clone(),
            ),
        )?;
        rpc_module.merge(rpc_api::node::RpcServer::into_rpc(
            rpc_server_impl.clone(),
        ))?;
        rpc_module
            .merge(rpc_api::wallet::RpcServer::into_rpc(rpc_server_impl))?;

        let server_addrs = ServerAddesses {
            _rpc_addr: rpc_server_addr,
            _private_rpc_addr: rpc_server_addr,
        };
        let handle = server.start(rpc_module);
        let task_handle = tokio::spawn(handle.stopped());
        (task_handle, server_addrs)
    };
    Ok(server_addrs)
}
