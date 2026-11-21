//! The Sidepot tab: the game as of the latest L1 block, this node's game account, and the three things a
//! player does: add funds, buy keys, cash out.

use std::{
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};

use eframe::egui::{self, Color32, RichText};
use parking_lot::Mutex;
use thunder::types::Address;

use crate::{
    app::App,
    play::{self, GameView},
};

const REFRESH: Duration = Duration::from_secs(5);
const BRASS: Color32 = Color32::from_rgb(0xd9, 0xb2, 0x3a);
const HOT: Color32 = Color32::from_rgb(0xff, 0x6b, 0x4a);
const DEFAULT_DEPOSIT_FEE_SATS: u64 = 1_000;

/// A value worked out off the UI thread: empty, then filled once.
type Slot<T> = Arc<Mutex<Option<Result<T, String>>>>;

/// One of the tab's actions: running or not, and how its last run ended.
#[derive(Default)]
struct Action {
    running: bool,
    result: Slot<String>,
}

impl Action {
    /// Runs `work` on the app's local pool; its outcome shows under the action's form.
    fn run<Fut>(
        &mut self,
        app: &App,
        work: impl FnOnce(App) -> Fut + Send + 'static,
    ) where
        Fut: Future<Output = Result<String, String>> + 'static,
    {
        self.running = true;
        *self.result.lock() = None;
        let result = self.result.clone();
        let app_handle = app.clone();
        app.local_pool.spawn_pinned(move || async move {
            let outcome = work(app_handle).await;
            if let Err(err) = &outcome {
                tracing::warn!("Sidepot action failed: {err}");
            }
            *result.lock() = Some(outcome);
        });
    }

    fn show_result(&mut self, ui: &mut egui::Ui) {
        let result = self.result.lock().clone();
        match result {
            None if self.running => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Sending to eCash…");
                });
            }
            None => (),
            Some(outcome) => {
                self.running = false;
                match outcome {
                    Ok(done) => ui.colored_label(BRASS, done),
                    Err(err) => ui.colored_label(HOT, err),
                };
            }
        }
    }
}

pub struct Sidepot {
    view: Slot<GameView>,
    /// The last view that loaded, kept on screen while a refresh fails.
    shown: Option<GameView>,
    stale: Option<String>,
    refreshing: bool,
    refreshed_at: Option<Instant>,
    keys: u16,
    deposit_amount: String,
    cashout_amount: String,
    cashout_dest: String,
    buy: Action,
    deposit: Action,
    cashout: Action,
}

impl Default for Sidepot {
    fn default() -> Self {
        Self {
            view: Slot::default(),
            shown: None,
            stale: None,
            refreshing: false,
            refreshed_at: None,
            keys: 1,
            deposit_amount: String::new(),
            cashout_amount: String::new(),
            cashout_dest: String::new(),
            buy: Action::default(),
            deposit: Action::default(),
            cashout: Action::default(),
        }
    }
}

fn ecx(sats: u64) -> String {
    format!("{}.{:08}", sats / 100_000_000, sats % 100_000_000)
}

/// An amount typed in ECX, as sats.
fn parse_ecx(text: &str) -> Result<u64, String> {
    bitcoin::Amount::from_str_in(text.trim(), bitcoin::Denomination::Bitcoin)
        .map(|amount| amount.to_sat())
        .map_err(|_| format!("\"{}\" is not an ECX amount", text.trim()))
}

fn err_text(err: crate::app::Error) -> String {
    format!("{:#}", anyhow::Error::from(err))
}

impl Sidepot {
    fn refresh(&mut self, app: &App) {
        if let Some(outcome) = self.view.lock().take() {
            self.refreshing = false;
            match outcome {
                Ok(view) => {
                    self.shown = Some(view);
                    self.stale = None;
                }
                Err(err) => self.stale = Some(err),
            }
        }
        let due = self
            .refreshed_at
            .is_none_or(|refreshed_at| refreshed_at.elapsed() >= REFRESH);
        if due && !self.refreshing {
            self.refreshing = true;
            self.refreshed_at = Some(Instant::now());
            let view = self.view.clone();
            let app_handle = app.clone();
            app.local_pool.spawn_pinned(move || async move {
                let outcome = app_handle.game_view().await.map_err(err_text);
                *view.lock() = Some(outcome);
            });
        }
    }

    fn show_game(view: &GameView, ui: &mut egui::Ui) {
        let game = &view.preview;
        ui.label(format!("Round {} pot", game.round));
        ui.label(
            RichText::new(format!("{} ECX", ecx(game.pot)))
                .size(44.0)
                .strong()
                .color(BRASS),
        );
        ui.label(format!(
            "The last block of buyers splits {} ECX. The rest opens the next round.",
            ecx(game.pot / 4 * 3 + game.pot % 4 * 3 / 4)
        ));
        ui.add_space(8.0);
        let left = view.blocks_left();
        let final_stretch = view.open() && left <= game.params.extension;
        let count = RichText::new(format!(
            "{left} {} left",
            if left == 1 { "block" } else { "blocks" }
        ))
        .size(26.0)
        .strong();
        ui.label(if final_stretch { count.color(HOT) } else { count });
        ui.label(
            "Every buy puts at least 6 blocks back on the clock. Empty eCash blocks don't count.",
        );
        if !view.open() {
            let to_go = view.start_height - view.tip_height;
            ui.add_space(4.0);
            ui.colored_label(
                BRASS,
                format!(
                    "The game opens at eCash block {}, {to_go} {} from now. You can add funds already; buys \
                     before that block don't count.",
                    view.start_height,
                    if to_go == 1 { "block" } else { "blocks" },
                ),
            );
        }
        ui.add_space(8.0);
        egui::Grid::new("sidepot_facts").num_columns(2).show(ui, |ui| {
            ui.label("Price per key");
            ui.monospace(format!("{} ECX", ecx(game.price)));
            ui.end_row();
            ui.label("Bonus pool");
            ui.monospace(format!("{} ECX", ecx(game.streak_pool)));
            ui.end_row();
            ui.label("Latest eCash block");
            ui.monospace(view.tip_height.to_string());
            ui.end_row();
            ui.label("Final up to block");
            ui.monospace(view.confirmed.height.to_string());
            ui.end_row();
        });
    }

    fn show_account(view: &GameView, ui: &mut egui::Ui) {
        ui.heading("Your game account");
        let Some(key) = view.key else {
            ui.label("Set the wallet's seed to get a game account.");
            return;
        };
        let account = view.account().unwrap_or_default();
        egui::Grid::new("sidepot_account").num_columns(2).show(ui, |ui| {
            ui.label("Balance");
            ui.monospace(format!("{} ECX", ecx(account.balance_sats)));
            ui.end_row();
            ui.label("Keys this round");
            ui.monospace(account.keys.to_string());
            ui.end_row();
        });
        let address = format!("spot:{}", const_hex::encode(key));
        ui.horizontal(|ui| {
            ui.label("Deposit address");
            if ui.small_button("Copy").clicked() {
                ui.ctx().copy_text(address.clone());
            }
        });
        ui.add(egui::Label::new(RichText::new(address).monospace()).wrap());
    }

    fn show_buy(&mut self, app: &App, view: &GameView, ui: &mut egui::Ui) {
        ui.heading("Buy keys");
        ui.horizontal(|ui| {
            ui.add(
                egui::DragValue::new(&mut self.keys).range(1..=1000).speed(0.2),
            );
            ui.label(if self.keys == 1 { "key" } else { "keys" });
        });
        let price = view.preview.price;
        let keys = u64::from(self.keys);
        egui::Grid::new("sidepot_buy").num_columns(2).show(ui, |ui| {
            ui.label("Cost now");
            ui.monospace(format!("{} ECX", ecx(keys.saturating_mul(price))));
            ui.end_row();
            ui.label("At most");
            ui.monospace(format!(
                "{} ECX",
                ecx(keys
                    .saturating_mul(play::default_max_price(&view.preview)))
            ));
            ui.end_row();
        });
        let step_ppm =
            view.preview.next_buy(&thunder::game::fork()).step_ppm;
        ui.label(format!(
            "The price rises after each block with a buy; the next such block raises it {}%. If it passes \
             your limit, or three counted blocks go by, the buy lapses and costs only the eCash fee.",
            step_ppm as f64 / 10_000.0
        ));
        let label = if view.open() {
            format!(
                "Buy {} {}",
                self.keys,
                if self.keys == 1 { "key" } else { "keys" }
            )
        } else {
            "Not open yet".to_owned()
        };
        let enabled = view.open() && !self.buy.running && view.key.is_some();
        if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
            let keys = self.keys;
            self.buy.run(app, move |app| async move {
                let sent =
                    app.game_buy(keys, None, None).await.map_err(err_text)?;
                Ok(format!(
                    "Sent in eCash transaction {}. It counts once a block includes it.",
                    sent.txid.map(|txid| txid.to_string()).unwrap_or_default()
                ))
            });
        }
        self.buy.show_result(ui);
    }

    fn show_deposit(&mut self, app: &App, view: &GameView, ui: &mut egui::Ui) {
        ui.heading("Add funds");
        ui.label(
            "Moves ECX from the enforcer's wallet into your game account. It shows as soon as an eCash \
             block includes it.",
        );
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.deposit_amount)
                    .hint_text("amount")
                    .desired_width(110.0),
            );
            ui.label("ECX");
        });
        let enabled = !self.deposit.running && view.key.is_some();
        if ui.add_enabled(enabled, egui::Button::new("Add funds")).clicked() {
            match parse_ecx(&self.deposit_amount) {
                Ok(0) => {
                    *self.deposit.result.lock() =
                        Some(Err("Enter an amount above zero.".to_owned()))
                }
                Ok(sats) => self.deposit.run(app, move |app| async move {
                    let txid = app
                        .game_deposit(sats, DEFAULT_DEPOSIT_FEE_SATS)
                        .await
                        .map_err(err_text)?;
                    Ok(format!("Deposit sent in eCash transaction {txid}."))
                }),
                Err(err) => *self.deposit.result.lock() = Some(Err(err)),
            }
        }
        self.deposit.show_result(ui);
    }

    fn show_cashout(&mut self, app: &App, view: &GameView, ui: &mut egui::Ui) {
        ui.heading("Cash out");
        ui.label(
            "Pays from your game account to a coin on the Sidepot sidechain (the Coins tab). Moving it on to \
             eCash is a drivechain withdrawal, which is slow.",
        );
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.cashout_amount)
                    .hint_text("all of it")
                    .desired_width(110.0),
            );
            ui.label("ECX");
        });
        ui.add(
            egui::TextEdit::singleline(&mut self.cashout_dest)
                .hint_text("to a new address of this wallet")
                .desired_width(300.0),
        );
        let enabled = !self.cashout.running && view.key.is_some();
        if ui.add_enabled(enabled, egui::Button::new("Cash out")).clicked() {
            let amount = if self.cashout_amount.trim().is_empty() {
                Ok(0)
            } else {
                parse_ecx(&self.cashout_amount).and_then(|sats| {
                    if sats == 0 {
                        Err("Leave the amount empty to cash out everything."
                            .to_owned())
                    } else {
                        Ok(sats)
                    }
                })
            };
            let dest = if self.cashout_dest.trim().is_empty() {
                Ok(None)
            } else {
                Address::from_str(self.cashout_dest.trim())
                    .map(Some)
                    .map_err(|_| "That is not a Sidepot address.".to_owned())
            };
            match (amount, dest) {
                (Ok(sats), Ok(dest)) => {
                    self.cashout.run(app, move |app| async move {
                        let sent = app
                            .game_cashout(sats, dest, None)
                            .await
                            .map_err(err_text)?;
                        let dest = sent
                            .dest
                            .map(|dest| dest.to_string())
                            .unwrap_or_default();
                        Ok(format!(
                            "Sent in eCash transaction {}. The coin arrives at {dest} a few blocks after \
                             eCash includes it.",
                            sent.txid.map(|txid| txid.to_string()).unwrap_or_default()
                        ))
                    })
                }
                (Err(err), _) | (_, Err(err)) => {
                    *self.cashout.result.lock() = Some(Err(err))
                }
            }
        }
        self.cashout.show_result(ui);
    }

    pub fn show(&mut self, app: Option<&App>, ui: &mut egui::Ui) {
        let Some(app) = app else {
            ui.label("The node is not running; see the Console / Logs tab.");
            return;
        };
        self.refresh(app);
        if let Some(stale) = &self.stale {
            ui.colored_label(
                HOT,
                format!("Can't read the game just now: {stale}"),
            );
        }
        let Some(view) = self.shown.clone() else {
            if self.stale.is_none() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Reading the game…");
                });
            }
            return;
        };
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add_space(6.0);
            ui.columns(2, |columns| {
                Self::show_game(&view, &mut columns[0]);
                Self::show_account(&view, &mut columns[1]);
            });
            ui.add_space(10.0);
            ui.separator();
            if app.game_wallet.is_none() {
                ui.colored_label(
                    HOT,
                    "The enforcer this node uses has no wallet, so it cannot post moves or deposits. Start \
                     the enforcer with --enable-wallet, or play from a wallet and sidepot.cash.",
                );
                return;
            }
            ui.columns(3, |columns| {
                self.show_buy(app, &view, &mut columns[0]);
                self.show_deposit(app, &view, &mut columns[1]);
                self.show_cashout(app, &view, &mut columns[2]);
            });
        });
    }
}
