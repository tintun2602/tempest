use crate::risk::Position;
use crate::strategy::{EntryConditions, IndicatorSnapshot};
use tracing::{debug, warn};

/// Telegram notifier. If `TELEGRAM_BOT_TOKEN` and `TELEGRAM_CHAT_ID` are not set,
/// all methods are silent no-ops — the bot runs fine without notifications.
pub struct Notifier {
    token: Option<String>,
    chat_id: Option<String>,
    quote_asset: String,
    http: reqwest::Client,
}

impl Notifier {
    pub fn from_env() -> Self {
        let token = std::env::var("TELEGRAM_BOT_TOKEN").ok();
        let chat_id = std::env::var("TELEGRAM_CHAT_ID").ok();
        let quote_asset = std::env::var("QUOTE_ASSET").unwrap_or_else(|_| "USDT".to_string());

        let notifier = Self {
            token,
            chat_id,
            quote_asset,
            http: reqwest::Client::new(),
        };

        if notifier.is_enabled() {
            tracing::info!("Telegram notifications enabled");
        } else {
            debug!("Telegram not configured — notifications disabled");
        }

        notifier
    }

    /// A notifier that never sends. Used by tests so no suite run can fire a
    /// real Telegram message from an operator's ambient environment.
    #[cfg(test)]
    pub fn disabled(quote_asset: &str) -> Self {
        Self {
            token: None,
            chat_id: None,
            quote_asset: quote_asset.to_string(),
            http: reqwest::Client::new(),
        }
    }

    pub fn quote_asset(&self) -> &str {
        &self.quote_asset
    }

    pub fn is_enabled(&self) -> bool {
        self.token.is_some() && self.chat_id.is_some()
    }

    /// Send a raw Markdown message. No-op if not configured.
    pub async fn send(&self, text: &str) {
        let (Some(token), Some(chat_id)) = (&self.token, &self.chat_id) else {
            return;
        };

        let url = format!("https://api.telegram.org/bot{token}/sendMessage");

        let result = self
            .http
            .post(&url)
            .json(&serde_json::json!({
                "chat_id": chat_id,
                "text": text,
                "parse_mode": "Markdown",
                "disable_web_page_preview": true
            }))
            .send()
            .await;

        match result {
            Ok(resp) if resp.status().is_success() => {
                debug!("Telegram message sent");
            }
            Ok(resp) => {
                warn!(
                    "Telegram API returned {}: {:?}",
                    resp.status(),
                    resp.text().await.ok()
                );
            }
            Err(e) => {
                warn!("Telegram send failed: {e}");
            }
        }
    }

    // -----------------------------------------------------------------------
    // Convenience methods
    // -----------------------------------------------------------------------

    pub async fn notify_buy(
        &self,
        symbol: &str,
        price: f64,
        qty: f64,
        stop_loss: f64,
        take_profit: f64,
        confidence: &str,
        reasoning: &str,
    ) {
        let msg = format!(
            "\u{1f7e2} *BUY {symbol}*\n\
             Price: `{price:.2}`\n\
             Qty: `{qty:.6}`\n\
             SL: `{stop_loss:.2}` | TP: `{take_profit:.2}`\n\
             Confidence: *{confidence}*\n\
             _{reasoning}_"
        );
        self.send(&msg).await;
    }

    pub async fn notify_sell(&self, symbol: &str, price: f64, pnl: f64, pnl_pct: f64) {
        let emoji = if pnl >= 0.0 { "\u{1f7e2}" } else { "\u{1f534}" };
        let msg = format!(
            "{emoji} *SELL {symbol}*\n\
             Price: `{price:.2}`\n\
             PnL: `{pnl:+.2}` {} (`{pnl_pct:+.2}%`)",
            self.quote_asset
        );
        self.send(&msg).await;
    }

    pub async fn notify_halt(&self, drawdown_pct: f64, equity: f64) {
        let msg = format!(
            "\u{1f6d1} *HALT — Daily Drawdown Limit*\n\
             Drawdown: `{drawdown_pct:.2}%`\n\
             Equity: `{equity:.2}` {}\n\
             _No new trades until next UTC midnight._",
            self.quote_asset
        );
        self.send(&msg).await;
    }

    pub async fn notify_reconcile(&self, restored: u32, emergency: u32, failed: u32) {
        if restored == 0 && emergency == 0 && failed == 0 {
            return;
        }
        let mut msg = String::from("\u{1f504} *Startup Reconciliation*\n");
        if restored > 0 {
            msg.push_str(&format!("Restored: {restored} position(s)\n"));
        }
        if emergency > 0 {
            msg.push_str(&format!("Emergency OCO: {emergency} order(s) placed\n"));
        }
        if failed > 0 {
            msg.push_str(&format!(
                "\u{26a0}\u{fe0f} *Failed: {failed}* — check exchange manually!"
            ));
        }
        self.send(&msg).await;
    }

    pub async fn notify_error(&self, context: &str, error: &str) {
        let msg = format!("\u{26a0}\u{fe0f} *Error: {context}*\n`{error}`");
        self.send(&msg).await;
    }

    pub async fn notify_startup(
        &self,
        equity: f64,
        free: f64,
        pairs: &[String],
        risk_per_trade: f64,
        poll_interval_secs: u64,
    ) {
        let msg = format!(
            "\u{1f680} *Tempest Started*\n\
             Equity: `{equity:.2}` {quote} (`{free:.2}` free)\n\
             Pairs: {pairs}\n\
             Risk/trade: `{risk:.2}%` \u{b7} Poll: every `{hours:.0}h`",
            quote = self.quote_asset,
            pairs = pairs.join(", "),
            risk = risk_per_trade * 100.0,
            hours = poll_interval_secs as f64 / 3600.0,
        );
        self.send(&msg).await;
    }

    /// Per-symbol state: either how close the setup is, or how an open
    /// position is doing.
    ///
    /// Signals average a fortnight apart, so without this the bot is silent for
    /// weeks and there is no way to tell "waiting correctly" from "wedged".
    pub async fn notify_status(
        &self,
        symbol: &str,
        snap: &IndicatorSnapshot,
        conditions: &EntryConditions,
        position: Option<&Position>,
    ) {
        let msg = match position {
            Some(pos) => Self::position_status(symbol, snap, pos),
            None => Self::entry_status(symbol, snap, conditions),
        };
        self.send(&msg).await;
    }

    fn entry_status(
        symbol: &str,
        snap: &IndicatorSnapshot,
        conditions: &EntryConditions,
    ) -> String {
        let mut msg = format!(
            "\u{1f440} *{symbol}* \u{2014} flat\n\
             Price `{price:.2}` \u{b7} RSI `{rsi:.1}`\n\
             EMA50 `{ema50:.2}` \u{b7} EMA200 `{ema200:.2}`\n\n",
            price = snap.current_price,
            rsi = snap.rsi_14,
            ema50 = snap.ema_50,
            ema200 = snap.ema_200,
        );

        for (label, met) in conditions.checklist() {
            let mark = if met { "\u{2705}" } else { "\u{274c}" };
            msg.push_str(&format!("{mark} {label}\n"));
        }

        msg.push_str(&format!(
            "\n*{}/4* conditions met",
            conditions.met_count()
        ));
        msg
    }

    fn position_status(symbol: &str, snap: &IndicatorSnapshot, pos: &Position) -> String {
        let price = snap.current_price;
        let pnl_pct = if pos.entry_price > 0.0 {
            (price - pos.entry_price) / pos.entry_price * 100.0
        } else {
            0.0
        };
        let to = |level: f64| {
            if level > 0.0 && price > 0.0 {
                format!("`{level:.2}` ({:+.1}%)", (level - price) / price * 100.0)
            } else {
                "_unset_".to_string()
            }
        };

        format!(
            "\u{1f4c8} *{symbol}* \u{2014} long\n\
             Entry `{entry:.2}` \u{b7} Now `{price:.2}` (`{pnl_pct:+.2}%`)\n\
             Qty `{qty:.8}`\n\
             SL {sl}\n\
             TP {tp}\n\
             Bracket: {bracket}",
            entry = pos.entry_price,
            qty = pos.quantity,
            sl = to(pos.stop_loss),
            tp = to(pos.take_profit),
            bracket = if pos.protected {
                "\u{2705} confirmed on exchange"
            } else {
                "\u{26a0}\u{fe0f} *UNPROTECTED*"
            },
        )
    }
}
