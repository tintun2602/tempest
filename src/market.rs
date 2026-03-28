use crate::config::Config;
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone)]
pub struct Candle {
    pub open_time: u64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    pub close_time: u64,
}

pub struct BinanceClient {
    base_url: String,
    api_key: String,
    api_secret: String,
    http: reqwest::Client,
}

impl BinanceClient {
    pub fn new(config: &Config) -> Self {
        Self {
            base_url: config.base_url.clone(),
            api_key: config.api_key.clone(),
            api_secret: config.api_secret.clone(),
            http: reqwest::Client::new(),
        }
    }

    fn timestamp_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("Time went backwards")
            .as_millis() as u64
    }

    fn sign(&self, query: &str) -> String {
        let mut mac =
            HmacSha256::new_from_slice(self.api_secret.as_bytes()).expect("HMAC accepts any key");
        mac.update(query.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    /// Fetch kline/candlestick data. `interval`: "1h", "4h", "1d", etc.
    pub async fn get_klines(
        &self,
        symbol: &str,
        interval: &str,
        limit: u32,
    ) -> Result<Vec<Candle>, String> {
        self.get_klines_from(&self.base_url, symbol, interval, limit).await
    }

    /// Fetch klines from the production Binance API (public, no auth needed).
    /// Useful for backtesting when the configured base_url is a testnet with limited history.
    pub async fn get_klines_public(
        &self,
        symbol: &str,
        interval: &str,
        limit: u32,
    ) -> Result<Vec<Candle>, String> {
        self.get_klines_from("https://api.binance.com", symbol, interval, limit).await
    }

    /// Fetch extended kline history by paginating backwards. Returns up to `total` candles.
    /// Uses the production API.
    pub async fn get_klines_extended(
        &self,
        symbol: &str,
        interval: &str,
        total: usize,
    ) -> Result<Vec<Candle>, String> {
        let base_url = "https://api.binance.com";
        let mut all_candles: Vec<Candle> = Vec::new();
        let mut end_time: Option<u64> = None;

        while all_candles.len() < total {
            let limit = 1000.min(total - all_candles.len()) as u32;
            let url = match end_time {
                Some(et) => format!(
                    "{base_url}/api/v3/klines?symbol={symbol}&interval={interval}&limit={limit}&endTime={et}"
                ),
                None => format!(
                    "{base_url}/api/v3/klines?symbol={symbol}&interval={interval}&limit={limit}"
                ),
            };

            let response: Vec<Vec<serde_json::Value>> = self
                .http
                .get(&url)
                .send()
                .await
                .map_err(|e| format!("HTTP error: {e}"))?
                .json()
                .await
                .map_err(|e| format!("JSON parse error: {e}"))?;

            if response.is_empty() {
                break;
            }

            let batch: Vec<Candle> = response
                .into_iter()
                .filter_map(|k| {
                    if k.len() < 7 { return None; }
                    Some(Candle {
                        open_time: k[0].as_u64()?,
                        open: k[1].as_str()?.parse().ok()?,
                        high: k[2].as_str()?.parse().ok()?,
                        low: k[3].as_str()?.parse().ok()?,
                        close: k[4].as_str()?.parse().ok()?,
                        volume: k[5].as_str()?.parse().ok()?,
                        close_time: k[6].as_u64()?,
                    })
                })
                .collect();

            if batch.is_empty() {
                break;
            }

            // Next page ends just before the earliest candle in this batch
            end_time = Some(batch[0].open_time.saturating_sub(1));

            // Prepend batch (we're walking backwards)
            let mut combined = batch;
            combined.append(&mut all_candles);
            all_candles = combined;
        }

        Ok(all_candles)
    }

    async fn get_klines_from(
        &self,
        base_url: &str,
        symbol: &str,
        interval: &str,
        limit: u32,
    ) -> Result<Vec<Candle>, String> {
        let url = format!(
            "{}/api/v3/klines?symbol={}&interval={}&limit={}",
            base_url, symbol, interval, limit
        );

        let response: Vec<Vec<serde_json::Value>> = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?
            .json()
            .await
            .map_err(|e| format!("JSON parse error: {e}"))?;

        let candles = response
            .into_iter()
            .filter_map(|k| {
                if k.len() < 7 {
                    return None;
                }
                Some(Candle {
                    open_time: k[0].as_u64()?,
                    open: k[1].as_str()?.parse().ok()?,
                    high: k[2].as_str()?.parse().ok()?,
                    low: k[3].as_str()?.parse().ok()?,
                    close: k[4].as_str()?.parse().ok()?,
                    volume: k[5].as_str()?.parse().ok()?,
                    close_time: k[6].as_u64()?,
                })
            })
            .collect();

        Ok(candles)
    }

    /// Get the current price for a symbol.
    pub async fn get_price(&self, symbol: &str) -> Result<f64, String> {
        let url = format!("{}/api/v3/ticker/price?symbol={}", self.base_url, symbol);
        let resp: serde_json::Value = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?
            .json()
            .await
            .map_err(|e| format!("JSON parse error: {e}"))?;

        resp["price"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| "Failed to parse price".to_string())
    }

    /// Get the free USDT balance from the account.
    pub async fn get_usdt_balance(&self) -> Result<f64, String> {
        let ts = self.timestamp_ms();
        let query = format!("timestamp={ts}");
        let sig = self.sign(&query);
        let url = format!(
            "{}/api/v3/account?{query}&signature={sig}",
            self.base_url
        );

        let resp: serde_json::Value = self
            .http
            .get(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?
            .json()
            .await
            .map_err(|e| format!("JSON parse error: {e}"))?;

        let balances = resp["balances"]
            .as_array()
            .ok_or_else(|| format!("No balances array in response: {resp}"))?;

        for b in balances {
            if b["asset"].as_str() == Some("USDT") {
                return b["free"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| "Failed to parse USDT balance".to_string());
            }
        }

        Ok(0.0)
    }

    /// Place a market order (BUY or SELL).
    pub async fn market_order(
        &self,
        symbol: &str,
        side: &str,
        quantity: f64,
    ) -> Result<serde_json::Value, String> {
        let ts = self.timestamp_ms();
        let qty_str = format_quantity(quantity);
        let query = format!(
            "symbol={symbol}&side={side}&type=MARKET&quantity={qty_str}&timestamp={ts}"
        );
        let sig = self.sign(&query);
        let body = format!("{query}&signature={sig}");
        let url = format!("{}/api/v3/order", self.base_url);

        self.http
            .post(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?
            .json()
            .await
            .map_err(|e| format!("JSON parse error: {e}"))
    }

    /// Place an OCO sell order (take-profit limit + stop-loss limit).
    pub async fn oco_sell(
        &self,
        symbol: &str,
        quantity: f64,
        take_profit_price: f64,
        stop_price: f64,
        stop_limit_price: f64,
    ) -> Result<serde_json::Value, String> {
        let ts = self.timestamp_ms();
        let qty_str = format_quantity(quantity);
        let tp_str = format_price(take_profit_price);
        let stop_str = format_price(stop_price);
        let stop_limit_str = format_price(stop_limit_price);

        let query = format!(
            "symbol={symbol}\
             &side=SELL\
             &quantity={qty_str}\
             &aboveType=LIMIT_MAKER\
             &abovePrice={tp_str}\
             &belowType=STOP_LOSS_LIMIT\
             &belowPrice={stop_limit_str}\
             &belowStopPrice={stop_str}\
             &belowTimeInForce=GTC\
             &timestamp={ts}"
        );
        let sig = self.sign(&query);
        let body = format!("{query}&signature={sig}");
        let url = format!("{}/api/v3/orderList/oco", self.base_url);

        self.http
            .post(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?
            .json()
            .await
            .map_err(|e| format!("JSON parse error: {e}"))
    }

    /// Cancel all open orders for a symbol.
    pub async fn cancel_open_orders(&self, symbol: &str) -> Result<serde_json::Value, String> {
        let ts = self.timestamp_ms();
        let query = format!("symbol={symbol}&timestamp={ts}");
        let sig = self.sign(&query);
        let body = format!("{query}&signature={sig}");
        let url = format!("{}/api/v3/openOrders", self.base_url);

        self.http
            .delete(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?
            .json()
            .await
            .map_err(|e| format!("JSON parse error: {e}"))
    }

    /// Get all open orders for a symbol.
    pub async fn get_open_orders(&self, symbol: &str) -> Result<Vec<serde_json::Value>, String> {
        let ts = self.timestamp_ms();
        let query = format!("symbol={symbol}&timestamp={ts}");
        let sig = self.sign(&query);
        let url = format!(
            "{}/api/v3/openOrders?{query}&signature={sig}",
            self.base_url
        );

        self.http
            .get(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?
            .json()
            .await
            .map_err(|e| format!("JSON parse error: {e}"))
    }

    /// Get all non-zero balances from the account (asset name → free amount).
    pub async fn get_nonzero_balances(&self) -> Result<Vec<(String, f64)>, String> {
        let ts = self.timestamp_ms();
        let query = format!("timestamp={ts}");
        let sig = self.sign(&query);
        let url = format!(
            "{}/api/v3/account?{query}&signature={sig}",
            self.base_url
        );

        let resp: serde_json::Value = self
            .http
            .get(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?
            .json()
            .await
            .map_err(|e| format!("JSON parse error: {e}"))?;

        let balances = resp["balances"]
            .as_array()
            .ok_or_else(|| format!("No balances array in response: {resp}"))?;

        let mut result = Vec::new();
        for b in balances {
            let asset = b["asset"].as_str().unwrap_or("");
            let free: f64 = b["free"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0);
            let locked: f64 = b["locked"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0);
            if free + locked > 0.0 && asset != "USDT" {
                result.push((asset.to_string(), free + locked));
            }
        }
        Ok(result)
    }
}

/// Format quantity with up to 6 decimal places, trimming trailing zeros.
fn format_quantity(qty: f64) -> String {
    let s = format!("{:.6}", qty);
    let s = s.trim_end_matches('0');
    let s = s.trim_end_matches('.');
    s.to_string()
}

/// Format price with up to 2 decimal places.
fn format_price(price: f64) -> String {
    format!("{:.2}", price)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn test_client() -> BinanceClient {
        let config = Config {
            base_url: "https://testnet.binance.vision".to_string(),
            api_key: "test_key".to_string(),
            api_secret: "test_secret".to_string(),
            trading_pairs: vec![],
            poll_interval_secs: 300,
            backtest_mode: false,
        };
        BinanceClient::new(&config)
    }

    #[test]
    fn test_sign_deterministic() {
        let client = test_client();
        let sig1 = client.sign("timestamp=1234567890");
        let sig2 = client.sign("timestamp=1234567890");
        assert_eq!(sig1, sig2);
    }

    #[test]
    fn test_sign_produces_64_hex_chars() {
        let client = test_client();
        let sig = client.sign("timestamp=1234567890");
        assert_eq!(sig.len(), 64);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_sign_different_inputs() {
        let client = test_client();
        let sig1 = client.sign("timestamp=1111111111");
        let sig2 = client.sign("timestamp=2222222222");
        assert_ne!(sig1, sig2);
    }

    #[test]
    fn test_timestamp_ms_is_milliseconds() {
        let client = test_client();
        let ts = client.timestamp_ms();
        // Should be in milliseconds: after 2020, before 2100
        let year_2020_ms: u64 = 1_577_836_800_000;
        let year_2100_ms: u64 = 4_102_444_800_000;
        assert!(ts > year_2020_ms);
        assert!(ts < year_2100_ms);
    }

    #[test]
    fn test_format_quantity() {
        assert_eq!(format_quantity(0.15), "0.15");
        assert_eq!(format_quantity(1.0), "1");
        assert_eq!(format_quantity(0.001234), "0.001234");
        assert_eq!(format_quantity(0.100000), "0.1");
    }

    #[test]
    fn test_format_price() {
        assert_eq!(format_price(50000.0), "50000.00");
        assert_eq!(format_price(123.456), "123.46");
    }
}
