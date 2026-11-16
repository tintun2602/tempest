//! Binance spot wallet and universal transfers, for profit withdrawals and
//! top-ups of the USDⓈ-M futures wallet.
//!
//! Spot and `/sapi` live on `api.binance.com`, not the futures host, but are
//! signed the same way (HMAC-SHA256 over the query/body, key in
//! `X-MBX-APIKEY`).
//!
//! Key permissions (Binance API docs):
//! - `POST /sapi/v1/asset/transfer`: "Permits Universal Transfer".
//! - `POST /api/v3/order`: spot trading ("Enable Spot & Margin Trading").
//! - `GET /api/v3/account`: reading.

use super::binance::parse_symbol_filters;
use super::binance::{check_api_error, format_decimal, parse_order_outcome, parse_str_f64, sign};
use super::{OrderOutcome, Side, SymbolFilters, TransferDirection, WalletExchange};
use std::collections::HashMap;
use std::sync::Mutex;

pub const SPOT_URL: &str = "https://api.binance.com";

pub struct BinanceWalletClient {
    base_url: String,
    api_key: String,
    api_secret: String,
    http: reqwest::Client,
    filter_cache: Mutex<HashMap<String, SymbolFilters>>,
}

impl BinanceWalletClient {
    pub fn new(base_url: &str, api_key: &str, api_secret: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            api_secret: api_secret.to_string(),
            http: reqwest::Client::builder()
                .local_address(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
                .build()
                .expect("Failed to build HTTP client"),
            filter_cache: Mutex::new(HashMap::new()),
        }
    }

    /// `SPOT_BASE_URL`, default production.
    pub fn base_url_from_env() -> String {
        std::env::var("SPOT_BASE_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| SPOT_URL.to_string())
    }

    fn timestamp_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("Time went backwards")
            .as_millis() as u64
    }

    /// A signed request; GET carries the query in the URL, POST in the body.
    async fn signed(
        &self,
        method: reqwest::Method,
        path: &str,
        params: &str,
    ) -> Result<serde_json::Value, String> {
        let ts = Self::timestamp_ms();
        let query = if params.is_empty() {
            format!("timestamp={ts}")
        } else {
            format!("{params}&timestamp={ts}")
        };
        let signed = format!("{query}&signature={}", sign(&self.api_secret, &query));
        let request = if method == reqwest::Method::POST {
            self.http
                .post(format!("{}{path}", self.base_url))
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(signed)
        } else {
            self.http
                .request(method, format!("{}{path}?{signed}", self.base_url))
        };
        let resp: serde_json::Value = request
            .header("X-MBX-APIKEY", &self.api_key)
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?
            .json()
            .await
            .map_err(|e| format!("JSON parse error: {e}"))?;
        check_api_error(&resp)?;
        Ok(resp)
    }
}

impl WalletExchange for BinanceWalletClient {
    async fn spot_free(&self, asset: &str) -> Result<f64, String> {
        let resp = self
            .signed(reqwest::Method::GET, "/api/v3/account", "")
            .await?;
        parse_spot_free(&resp, asset)
    }

    async fn universal_transfer(
        &self,
        direction: TransferDirection,
        asset: &str,
        amount: f64,
    ) -> Result<u64, String> {
        let params = format!(
            "type={}&asset={asset}&amount={}",
            direction.wire(),
            format_decimal(amount)
        );
        let resp = self
            .signed(reqwest::Method::POST, "/sapi/v1/asset/transfer", &params)
            .await?;
        parse_tran_id(&resp)
    }

    async fn spot_price(&self, symbol: &str) -> Result<f64, String> {
        let resp: serde_json::Value = self
            .http
            .get(format!(
                "{}/api/v3/ticker/price?symbol={symbol}",
                self.base_url
            ))
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?
            .json()
            .await
            .map_err(|e| format!("JSON parse error: {e}"))?;
        check_api_error(&resp)?;
        parse_str_f64(&resp["price"])
            .filter(|p| *p > 0.0)
            .ok_or_else(|| format!("no price for {symbol}: {resp}"))
    }

    async fn sell_base(&self, symbol: &str, quantity: f64) -> Result<OrderOutcome, String> {
        let params = format!(
            "symbol={symbol}&side=SELL&type=MARKET&quantity={}&newOrderRespType=FULL",
            format_decimal(quantity)
        );
        let resp = self
            .signed(reqwest::Method::POST, "/api/v3/order", &params)
            .await?;
        Ok(parse_order_outcome(&resp, symbol, Side::Sell, quantity))
    }

    async fn spot_filters(&self, symbol: &str) -> Result<SymbolFilters, String> {
        if let Some(cached) = self
            .filter_cache
            .lock()
            .ok()
            .and_then(|c| c.get(symbol).cloned())
        {
            return Ok(cached);
        }
        let resp: serde_json::Value = self
            .http
            .get(format!(
                "{}/api/v3/exchangeInfo?symbol={symbol}",
                self.base_url
            ))
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?
            .json()
            .await
            .map_err(|e| format!("JSON parse error: {e}"))?;
        check_api_error(&resp)?;
        let filters = parse_symbol_filters(&resp, symbol)?;
        if let Ok(mut cache) = self.filter_cache.lock() {
            cache.insert(symbol.to_string(), filters.clone());
        }
        Ok(filters)
    }

    async fn buy_with_quote(
        &self,
        symbol: &str,
        quote_amount: f64,
    ) -> Result<OrderOutcome, String> {
        let params = format!(
            "symbol={symbol}&side=BUY&type=MARKET&quoteOrderQty={}&newOrderRespType=FULL",
            format_decimal(quote_amount)
        );
        let resp = self
            .signed(reqwest::Method::POST, "/api/v3/order", &params)
            .await?;
        Ok(parse_order_outcome(&resp, symbol, Side::Buy, 0.0))
    }
}

fn parse_tran_id(resp: &serde_json::Value) -> Result<u64, String> {
    resp["tranId"]
        .as_u64()
        .ok_or_else(|| format!("no tranId in transfer response: {resp}"))
}

fn parse_spot_free(resp: &serde_json::Value, asset: &str) -> Result<f64, String> {
    let balances = resp["balances"]
        .as_array()
        .ok_or_else(|| format!("no balances in spot account: {resp}"))?;
    Ok(balances
        .iter()
        .find(|b| b["asset"].as_str() == Some(asset))
        .and_then(|b| parse_str_f64(&b["free"]))
        .unwrap_or(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn transfer_types_are_binances_names() {
        assert_eq!(TransferDirection::FuturesToSpot.wire(), "UMFUTURE_MAIN");
        assert_eq!(TransferDirection::SpotToFutures.wire(), "MAIN_UMFUTURE");
    }

    #[test]
    fn the_transfer_id_is_read_from_the_response() {
        assert_eq!(
            parse_tran_id(&json!({"tranId": 13526853623u64})).unwrap(),
            13526853623
        );
        assert!(parse_tran_id(&json!({"x": 1})).is_err());
    }

    #[test]
    fn free_spot_balance_is_read_and_missing_means_zero() {
        let resp = json!({"balances": [
            {"asset": "USDC", "free": "12.50", "locked": "1.00"},
            {"asset": "BNB", "free": "0.2", "locked": "0"}
        ]});
        assert_eq!(parse_spot_free(&resp, "USDC").unwrap(), 12.5);
        assert_eq!(parse_spot_free(&resp, "ETH").unwrap(), 0.0);
        assert!(parse_spot_free(&json!({}), "USDC").is_err());
    }
}
