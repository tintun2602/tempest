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

        if token.is_some() && chat_id.is_some() {
            tracing::info!("Telegram notifications enabled");
        } else {
            debug!("Telegram not configured — notifications disabled");
        }

        Self {
            token,
            chat_id,
            quote_asset,
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
                warn!("Telegram API returned {}: {:?}", resp.status(), resp.text().await.ok());
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

    pub async fn notify_sell(
        &self,
        symbol: &str,
        price: f64,
        pnl: f64,
        pnl_pct: f64,
    ) {
        let emoji = if pnl >= 0.0 { "\u{1f7e2}" } else { "\u{1f534}" };
        let msg = format!(
            "{emoji} *SELL {symbol}*\n\
             Price: `{price:.2}`\n\
             PnL: `{pnl:+.2}` {} (`{pnl_pct:+.2}%`)"
            , self.quote_asset
        );
        self.send(&msg).await;
    }

    pub async fn notify_halt(&self, drawdown_pct: f64, equity: f64) {
        let msg = format!(
            "\u{1f6d1} *HALT — Daily Drawdown Limit*\n\
             Drawdown: `{drawdown_pct:.2}%`\n\
             Equity: `{equity:.2}` {}\n\
             _No new trades until next UTC midnight._"
            , self.quote_asset
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
            msg.push_str(&format!("\u{26a0}\u{fe0f} *Failed: {failed}* — check exchange manually!"));
        }
        self.send(&msg).await;
    }

    pub async fn notify_error(&self, context: &str, error: &str) {
        let msg = format!(
            "\u{26a0}\u{fe0f} *Error: {context}*\n`{error}`"
        );
        self.send(&msg).await;
    }

    pub async fn notify_startup(&self, equity: f64, pairs: &[String]) {
        let pairs_str = pairs.join(", ");
        let msg = format!(
            "\u{1f680} *Bot Started*\n\
             Equity: `{equity:.2}` {}\n\
             Pairs: {pairs_str}"
            , self.quote_asset
        );
        self.send(&msg).await;
    }
}
