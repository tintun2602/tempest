use std::env;

pub struct Config {
    pub base_url: String,
    pub api_key: String,
    pub api_secret: String,
    pub quote_asset: String,
    pub trading_pairs: Vec<String>,
    pub poll_interval_secs: u64,
    pub backtest_mode: bool,
    /// Fraction of equity risked per trade.
    pub risk_per_trade: f64,
}

impl Config {
    pub fn from_env() -> Self {
        dotenvy::dotenv().ok();

        let base_url = env::var("BINANCE_BASE_URL")
            .unwrap_or_else(|_| "https://testnet.binance.vision".to_string());
        let api_key = env::var("BINANCE_API_KEY").expect("BINANCE_API_KEY must be set");
        let api_secret = env::var("BINANCE_API_SECRET").expect("BINANCE_API_SECRET must be set");
        let quote_asset = env::var("QUOTE_ASSET").unwrap_or_else(|_| "USDT".to_string());

        let pairs_str = env::var("TRADING_PAIRS")
            .unwrap_or_else(|_| "BTCUSDT,ETHUSDT,SOLUSDT".to_string());
        let trading_pairs: Vec<String> =
            pairs_str.split(',').map(|s| s.trim().to_string()).collect();

        let poll_interval_secs: u64 = env::var("POLL_INTERVAL_SECONDS")
            .unwrap_or_else(|_| "300".to_string())
            .parse()
            .expect("POLL_INTERVAL_SECONDS must be a number");

        let log_level = env::var("LOG_LEVEL").unwrap_or_else(|_| "info".to_string());
        if env::var("RUST_LOG").is_err() {
            unsafe { env::set_var("RUST_LOG", &log_level) };
        }

        let backtest_mode = env::var("MODE").unwrap_or_default() == "backtest"
            || env::args().any(|a| a == "--backtest");

        // Percent, so RISK_PER_TRADE_PCT=5 means 5% of equity per trade.
        let risk_per_trade = env::var("RISK_PER_TRADE_PCT")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .map(|pct| pct / 100.0)
            .unwrap_or(crate::risk::DEFAULT_RISK_PER_TRADE);

        Self {
            base_url,
            api_key,
            api_secret,
            quote_asset,
            trading_pairs,
            poll_interval_secs,
            backtest_mode,
            risk_per_trade,
        }
    }
}
