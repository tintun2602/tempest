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

/// Every configured symbol must be quoted in `quote_asset`.
///
/// Deploying `TRADING_PAIRS=BTCUSDT` against a USDC account is not a runtime
/// error anywhere else — the bot reads a zero USDC balance, sizes nothing, and
/// looks merely idle.
fn validate_pairs(quote_asset: &str, pairs: &[String]) -> Result<(), String> {
    if quote_asset.is_empty() {
        return Err("QUOTE_ASSET must not be empty".to_string());
    }
    if pairs.is_empty() {
        return Err("TRADING_PAIRS must list at least one symbol".to_string());
    }

    let mismatched: Vec<&String> = pairs
        .iter()
        .filter(|symbol| {
            // The base must be non-empty too, so "USDC" alone is rejected.
            !symbol.ends_with(quote_asset) || symbol.len() <= quote_asset.len()
        })
        .collect();

    if mismatched.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "QUOTE_ASSET is {quote_asset}, but these symbols are not quoted in it: {mismatched:?}. \
             Either change QUOTE_ASSET or fix TRADING_PAIRS."
        ))
    }
}

impl Config {
    pub fn from_env() -> Self {
        dotenvy::dotenv().ok();

        let base_url = env::var("BINANCE_BASE_URL")
            .unwrap_or_else(|_| "https://testnet.binance.vision".to_string());
        let api_key = env::var("BINANCE_API_KEY").expect("BINANCE_API_KEY must be set");
        let api_secret = env::var("BINANCE_API_SECRET").expect("BINANCE_API_SECRET must be set");
        let quote_asset = env::var("QUOTE_ASSET").unwrap_or_else(|_| "USDT".to_string());

        let pairs_str = env::var("TRADING_PAIRS").unwrap_or_else(|_| format!("BTC{quote_asset}"));
        let trading_pairs: Vec<String> = pairs_str
            .split(',')
            .map(|s| s.trim().to_uppercase())
            .filter(|s| !s.is_empty())
            .collect();

        // A symbol quoted in a different asset silently trades a pair the
        // account cannot fund: balances are read in `quote_asset`, so the bot
        // would see zero free cash and price equity against the wrong market.
        if let Err(e) = validate_pairs(&quote_asset, &trading_pairs) {
            panic!("Invalid configuration: {e}");
        }

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

#[cfg(test)]
mod tests {
    use super::validate_pairs;

    fn pairs(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn matching_quote_asset_is_accepted() {
        assert!(validate_pairs("USDC", &pairs(&["BTCUSDC", "ETHUSDC"])).is_ok());
        assert!(validate_pairs("USDT", &pairs(&["BTCUSDT"])).is_ok());
    }

    #[test]
    fn mismatched_quote_asset_is_rejected() {
        // The exact bug shipped in fly.toml: a USDT pair on a USDC account.
        let err = validate_pairs("USDC", &pairs(&["BTCUSDT"])).unwrap_err();
        assert!(err.contains("BTCUSDT"), "got: {err}");
        assert!(err.contains("USDC"), "got: {err}");
    }

    #[test]
    fn one_bad_symbol_fails_the_whole_set() {
        let err = validate_pairs("USDC", &pairs(&["BTCUSDC", "ETHUSDT"])).unwrap_err();
        assert!(err.contains("ETHUSDT"));
        assert!(!err.contains("BTCUSDC"), "should only name the offender");
    }

    #[test]
    fn a_bare_quote_asset_is_not_a_symbol() {
        // "USDC" ends with "USDC" but names no base asset.
        assert!(validate_pairs("USDC", &pairs(&["USDC"])).is_err());
    }

    #[test]
    fn empty_configuration_is_rejected() {
        assert!(validate_pairs("USDC", &[]).is_err());
        assert!(validate_pairs("", &pairs(&["BTCUSDC"])).is_err());
    }

    #[test]
    fn quote_asset_must_be_a_suffix_not_a_substring() {
        // USDC appears in the symbol but is not what it is quoted in.
        assert!(validate_pairs("USDC", &pairs(&["USDCBTC"])).is_err());
    }
}
