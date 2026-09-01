//! Binance Spot REST adapter.
//!
//! Owns every detail of Binance's wire format — HMAC signing, stringly-typed
//! numbers, order-list semantics — and exposes it only through the provider
//! traits in the parent module.

use super::{
    weighted_average_price, AccountProvider, AccountSnapshot, Balance, Candle, ExecutionProvider,
    Fill, InstrumentProvider, MarketDataProvider, OcoPlacement, OpenOrder, OrderKind, OrderOutcome,
    Side, StopPlacement, SymbolFilters,
};
use crate::config::Config;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Mutex;

type HmacSha256 = Hmac<Sha256>;

/// Public Binance endpoint used for historical klines. The configured
/// `base_url` may be a testnet, which carries far too little history to warm up
/// an EMA200 or run a backtest.
const HISTORY_BASE_URL: &str = "https://api.binance.com";

/// Largest page the klines endpoint will return.
const MAX_KLINES_PER_REQUEST: usize = 1000;

pub struct BinanceClient {
    base_url: String,
    api_key: String,
    api_secret: String,
    http: reqwest::Client,
    /// Trading rules are effectively static, and every order needs them, so
    /// they are fetched once per symbol and kept.
    filter_cache: Mutex<HashMap<String, SymbolFilters>>,
}

impl BinanceClient {
    pub fn new(config: &Config) -> Self {
        Self {
            base_url: config.base_url.clone(),
            api_key: config.api_key.clone(),
            api_secret: config.api_secret.clone(),
            http: reqwest::Client::builder()
                .local_address(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
                .build()
                .expect("Failed to build HTTP client"),
            filter_cache: Mutex::new(HashMap::new()),
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

    /// GET a signed endpoint, returning the decoded body.
    async fn signed_get(&self, path: &str, query: &str) -> Result<serde_json::Value, String> {
        let sig = self.sign(query);
        let url = format!("{}{path}?{query}&signature={sig}", self.base_url);
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

    /// Send a signed form-encoded request, returning the decoded body.
    async fn signed_form(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &str,
    ) -> Result<serde_json::Value, String> {
        let sig = self.sign(query);
        let url = format!("{}{path}", self.base_url);
        self.http
            .request(method, &url)
            .header("X-MBX-APIKEY", &self.api_key)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(format!("{query}&signature={sig}"))
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?
            .json()
            .await
            .map_err(|e| format!("JSON parse error: {e}"))
    }

    async fn fetch_klines(
        &self,
        base_url: &str,
        symbol: &str,
        interval: &str,
        limit: u32,
        end_time: Option<u64>,
    ) -> Result<Vec<Candle>, String> {
        let mut url =
            format!("{base_url}/api/v3/klines?symbol={symbol}&interval={interval}&limit={limit}");
        if let Some(end) = end_time {
            url.push_str(&format!("&endTime={end}"));
        }

        let response: serde_json::Value = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?
            .json()
            .await
            .map_err(|e| format!("JSON parse error: {e}"))?;

        // A klines error comes back as an object, not the expected array.
        let rows = response
            .as_array()
            .ok_or_else(|| format!("klines request failed: {response}"))?;

        Ok(rows.iter().filter_map(parse_candle).collect())
    }
}

impl InstrumentProvider for BinanceClient {
    async fn filters(&self, symbol: &str) -> Result<SymbolFilters, String> {
        // Lock is released before the await — never held across it.
        if let Some(cached) = self
            .filter_cache
            .lock()
            .expect("filter cache mutex poisoned")
            .get(symbol)
        {
            return Ok(cached.clone());
        }

        let url = format!("{}/api/v3/exchangeInfo?symbol={symbol}", self.base_url);
        let resp: serde_json::Value = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?
            .json()
            .await
            .map_err(|e| format!("JSON parse error: {e}"))?;
        check_api_error(&resp)?;

        let filters = parse_symbol_filters(&resp, symbol)?;
        self.filter_cache
            .lock()
            .expect("filter cache mutex poisoned")
            .insert(symbol.to_string(), filters.clone());
        Ok(filters)
    }
}

impl MarketDataProvider for BinanceClient {
    async fn klines(
        &self,
        symbol: &str,
        interval: &str,
        limit: u32,
    ) -> Result<Vec<Candle>, String> {
        self.fetch_klines(&self.base_url, symbol, interval, limit, None)
            .await
    }

    /// Walk backwards from the present, one page at a time, until `total`
    /// candles are collected or the venue stops returning data. Always reads
    /// the public production endpoint — see [`HISTORY_BASE_URL`].
    async fn klines_extended(
        &self,
        symbol: &str,
        interval: &str,
        total: usize,
    ) -> Result<Vec<Candle>, String> {
        let mut all: Vec<Candle> = Vec::new();
        let mut end_time: Option<u64> = None;

        while all.len() < total {
            let limit = MAX_KLINES_PER_REQUEST.min(total - all.len()) as u32;
            let batch = self
                .fetch_klines(HISTORY_BASE_URL, symbol, interval, limit, end_time)
                .await?;

            let Some(earliest) = batch.first() else {
                break;
            };
            // Next page ends just before the earliest candle in this batch.
            end_time = Some(earliest.open_time.saturating_sub(1));

            let mut combined = batch;
            combined.append(&mut all);
            all = combined;
        }

        Ok(all)
    }

    async fn price(&self, symbol: &str) -> Result<f64, String> {
        let url = format!("{}/api/v3/ticker/price?symbol={symbol}", self.base_url);
        let resp: serde_json::Value = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?
            .json()
            .await
            .map_err(|e| format!("JSON parse error: {e}"))?;

        parse_str_f64(&resp["price"])
            .ok_or_else(|| format!("Failed to parse price for {symbol}: {resp}"))
    }
}

impl AccountProvider for BinanceClient {
    async fn account(&self, quote_asset: &str) -> Result<AccountSnapshot, String> {
        let ts = self.timestamp_ms();
        let resp = self
            .signed_get("/api/v3/account", &format!("timestamp={ts}"))
            .await?;
        parse_account(&resp, quote_asset)
    }
}

impl ExecutionProvider for BinanceClient {
    async fn market_order(
        &self,
        symbol: &str,
        side: Side,
        quantity: f64,
    ) -> Result<OrderOutcome, String> {
        let ts = self.timestamp_ms();
        let qty_str = format_quantity(quantity);
        let query = format!(
            "symbol={symbol}&side={}&type=MARKET&quantity={qty_str}\
             &newOrderRespType=FULL&timestamp={ts}",
            side.as_str()
        );

        let resp = self
            .signed_form(reqwest::Method::POST, "/api/v3/order", &query)
            .await?;
        check_api_error(&resp)?;
        Ok(parse_order_outcome(&resp, symbol, side, quantity))
    }

    async fn place_oco_sell(
        &self,
        symbol: &str,
        quantity: f64,
        take_profit_price: f64,
        stop_price: f64,
        stop_limit_price: f64,
    ) -> Result<OcoPlacement, String> {
        let ts = self.timestamp_ms();
        let query = format!(
            "symbol={symbol}\
             &side=SELL\
             &quantity={}\
             &aboveType=LIMIT_MAKER\
             &abovePrice={}\
             &belowType=STOP_LOSS_LIMIT\
             &belowPrice={}\
             &belowStopPrice={}\
             &belowTimeInForce=GTC\
             &timestamp={ts}",
            format_quantity(quantity),
            format_price(take_profit_price),
            format_price(stop_limit_price),
            format_price(stop_price),
        );

        let resp = self
            .signed_form(reqwest::Method::POST, "/api/v3/orderList/oco", &query)
            .await?;
        check_api_error(&resp)?;

        // A position is only protected once the venue hands back an order-list
        // id. Anything else — including a 200 with an unexpected shape — leaves
        // the stop unplaced, and the caller must be told so.
        let order_list_id = resp["orderListId"]
            .as_i64()
            .ok_or_else(|| format!("OCO not confirmed by exchange: {resp}"))?;

        Ok(OcoPlacement {
            order_list_id,
            stop_price,
            take_profit_price,
        })
    }

    async fn place_stop_loss(
        &self,
        symbol: &str,
        quantity: f64,
        stop_price: f64,
        stop_limit_price: f64,
    ) -> Result<StopPlacement, String> {
        let ts = self.timestamp_ms();
        let query = format!(
            "symbol={symbol}&side=SELL&type=STOP_LOSS_LIMIT\
             &quantity={}&price={}&stopPrice={}&timeInForce=GTC&timestamp={ts}",
            format_quantity(quantity),
            format_price(stop_limit_price),
            format_price(stop_price),
        );

        let resp = self
            .signed_form(reqwest::Method::POST, "/api/v3/order", &query)
            .await?;
        check_api_error(&resp)?;

        // As with the OCO, only a venue-issued id counts as protection.
        let order_id = resp["orderId"]
            .as_u64()
            .ok_or_else(|| format!("stop not confirmed by exchange: {resp}"))?;

        Ok(StopPlacement {
            order_id,
            stop_price,
        })
    }

    async fn cancel_open_orders(&self, symbol: &str) -> Result<(), String> {
        let ts = self.timestamp_ms();
        let resp = self
            .signed_form(
                reqwest::Method::DELETE,
                "/api/v3/openOrders",
                &format!("symbol={symbol}&timestamp={ts}"),
            )
            .await?;
        check_api_error(&resp)?;
        Ok(())
    }

    async fn open_orders(&self, symbol: &str) -> Result<Vec<OpenOrder>, String> {
        let ts = self.timestamp_ms();
        let resp = self
            .signed_get(
                "/api/v3/openOrders",
                &format!("symbol={symbol}&timestamp={ts}"),
            )
            .await?;
        check_api_error(&resp)?;

        let rows = resp
            .as_array()
            .ok_or_else(|| format!("openOrders returned unexpected shape: {resp}"))?;

        Ok(rows.iter().filter_map(parse_open_order).collect())
    }
}

// ---------------------------------------------------------------------------
// Wire-format parsing
// ---------------------------------------------------------------------------

/// Binance reports failures as `{"code": -1013, "msg": "..."}` with an HTTP 200
/// in some paths, so the body must be inspected rather than the status alone.
fn check_api_error(resp: &serde_json::Value) -> Result<(), String> {
    match resp.get("code").and_then(serde_json::Value::as_i64) {
        Some(code) if code != 0 => {
            let msg = resp["msg"].as_str().unwrap_or("unknown error");
            Err(format!("Binance API error {code}: {msg}"))
        }
        _ => Ok(()),
    }
}

/// Binance encodes every number as a JSON string.
fn parse_str_f64(v: &serde_json::Value) -> Option<f64> {
    v.as_str()?.parse().ok()
}

fn parse_str_f64_or_zero(v: &serde_json::Value) -> f64 {
    parse_str_f64(v).unwrap_or(0.0)
}

/// A kline row is a positional array: `[openTime, o, h, l, c, v, closeTime, ..]`.
fn parse_candle(row: &serde_json::Value) -> Option<Candle> {
    let k = row.as_array()?;
    if k.len() < 7 {
        return None;
    }
    Some(Candle {
        open_time: k[0].as_u64()?,
        open: parse_str_f64(&k[1])?,
        high: parse_str_f64(&k[2])?,
        low: parse_str_f64(&k[3])?,
        close: parse_str_f64(&k[4])?,
        volume: parse_str_f64(&k[5])?,
        close_time: k[6].as_u64()?,
    })
}

fn parse_account(resp: &serde_json::Value, quote_asset: &str) -> Result<AccountSnapshot, String> {
    let balances = resp["balances"]
        .as_array()
        .ok_or_else(|| format!("No balances array in response: {resp}"))?;

    let mut free_quote = 0.0;
    let mut assets = Vec::new();

    for b in balances {
        let asset = b["asset"].as_str().unwrap_or("");
        let free = parse_str_f64_or_zero(&b["free"]);
        let locked = parse_str_f64_or_zero(&b["locked"]);

        if asset == quote_asset {
            free_quote = free;
        } else if free + locked > 0.0 {
            // free + locked: an asset reserved by a resting OCO is still ours.
            assets.push(Balance {
                asset: asset.to_string(),
                quantity: free + locked,
            });
        }
    }

    Ok(AccountSnapshot { free_quote, assets })
}

/// Normalise a FULL market-order response.
///
/// `requested_quantity` is the fallback when the venue omits `executedQty`.
/// `average_price` is `0.0` when no fill information is recoverable; callers
/// decide what to substitute rather than being handed a fabricated price.
fn parse_order_outcome(
    resp: &serde_json::Value,
    symbol: &str,
    side: Side,
    requested_quantity: f64,
) -> OrderOutcome {
    let fills: Vec<Fill> = resp["fills"]
        .as_array()
        .map(|fills| {
            fills
                .iter()
                .filter_map(|f| {
                    Some(Fill {
                        price: parse_str_f64(&f["price"])?,
                        quantity: parse_str_f64(&f["qty"])?,
                        commission: parse_str_f64_or_zero(&f["commission"]),
                        commission_asset: f["commissionAsset"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let filled_quantity = parse_str_f64(&resp["executedQty"]).unwrap_or(requested_quantity);

    // Prefer the VWAP of the individual fills; fall back to the aggregate the
    // venue reports.
    let average_price = weighted_average_price(&fills).unwrap_or_else(|| {
        let quote_qty = parse_str_f64_or_zero(&resp["cummulativeQuoteQty"]);
        let exec_qty = parse_str_f64_or_zero(&resp["executedQty"]);
        if exec_qty > 0.0 {
            quote_qty / exec_qty
        } else {
            0.0
        }
    });

    OrderOutcome {
        symbol: symbol.to_string(),
        side,
        filled_quantity,
        average_price,
        fills,
    }
}

/// Pull the lot/price/notional rules out of an `exchangeInfo` response.
fn parse_symbol_filters(resp: &serde_json::Value, symbol: &str) -> Result<SymbolFilters, String> {
    let entry = resp["symbols"]
        .as_array()
        .and_then(|symbols| {
            symbols
                .iter()
                .find(|s| s["symbol"].as_str() == Some(symbol))
        })
        .ok_or_else(|| format!("exchangeInfo has no entry for {symbol}"))?;

    let find = |kind: &str| {
        entry["filters"].as_array().and_then(|filters| {
            filters
                .iter()
                .find(|f| f["filterType"].as_str() == Some(kind))
                .cloned()
        })
    };

    let lot =
        find("LOT_SIZE").ok_or_else(|| format!("{symbol}: exchangeInfo has no LOT_SIZE filter"))?;
    let price = find("PRICE_FILTER")
        .ok_or_else(|| format!("{symbol}: exchangeInfo has no PRICE_FILTER"))?;
    // Older symbols expose MIN_NOTIONAL; newer ones expose NOTIONAL.
    let notional = find("NOTIONAL").or_else(|| find("MIN_NOTIONAL"));

    Ok(SymbolFilters {
        symbol: symbol.to_string(),
        base_asset: entry["baseAsset"].as_str().unwrap_or_default().to_string(),
        quote_asset: entry["quoteAsset"].as_str().unwrap_or_default().to_string(),
        step_size: parse_str_f64_or_zero(&lot["stepSize"]),
        min_qty: parse_str_f64_or_zero(&lot["minQty"]),
        max_qty: parse_str_f64_or_zero(&lot["maxQty"]),
        tick_size: parse_str_f64_or_zero(&price["tickSize"]),
        min_notional: notional
            .as_ref()
            .map(|n| parse_str_f64_or_zero(&n["minNotional"]))
            .unwrap_or(0.0),
    })
}

fn parse_open_order(o: &serde_json::Value) -> Option<OpenOrder> {
    Some(OpenOrder {
        order_id: o["orderId"].as_u64().unwrap_or(0),
        symbol: o["symbol"].as_str()?.to_string(),
        side: match o["side"].as_str()? {
            "BUY" => Side::Buy,
            "SELL" => Side::Sell,
            _ => return None,
        },
        kind: OrderKind::from_wire(o["type"].as_str().unwrap_or("")),
        price: parse_str_f64_or_zero(&o["price"]),
        stop_price: parse_str_f64_or_zero(&o["stopPrice"]),
        quantity: parse_str_f64_or_zero(&o["origQty"]),
    })
}

/// Render a number at the venue's full base precision, trimming trailing
/// zeros.
///
/// Callers are expected to have already rounded to the symbol's step or tick
/// via [`SymbolFilters`]; this only has to avoid *losing* that precision.
/// Fixed 6-decimal quantities silently violated a 1e-5 step size, and fixed
/// 2-decimal prices would destroy the price of any sub-cent asset.
fn format_decimal(value: f64) -> String {
    let text = format!("{value:.8}");
    let text = text.trim_end_matches('0');
    text.trim_end_matches('.').to_string()
}

fn format_quantity(qty: f64) -> String {
    format_decimal(qty)
}

fn format_price(price: f64) -> String {
    format_decimal(price)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exchange::protective_levels;
    use serde_json::json;

    fn test_client() -> BinanceClient {
        BinanceClient::new(&Config {
            base_url: "https://testnet.binance.vision".to_string(),
            api_key: "test_key".to_string(),
            api_secret: "test_secret".to_string(),
            quote_asset: "USDT".to_string(),
            trading_pairs: vec![],
            poll_interval_secs: 300,
            backtest_mode: false,
            risk: Default::default(),
        })
    }

    // ----- signing -----

    #[test]
    fn sign_is_deterministic() {
        let client = test_client();
        assert_eq!(
            client.sign("timestamp=1234567890"),
            client.sign("timestamp=1234567890")
        );
    }

    #[test]
    fn sign_produces_64_hex_chars() {
        let sig = test_client().sign("timestamp=1234567890");
        assert_eq!(sig.len(), 64);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn sign_differs_for_different_inputs() {
        let client = test_client();
        assert_ne!(
            client.sign("timestamp=1111111111"),
            client.sign("timestamp=2222222222")
        );
    }

    #[test]
    fn timestamp_is_in_milliseconds() {
        let ts = test_client().timestamp_ms();
        // After 2020, before 2100.
        assert!(ts > 1_577_836_800_000);
        assert!(ts < 4_102_444_800_000);
    }

    // ----- formatting -----

    #[test]
    fn quantity_formatting_trims_trailing_zeros() {
        assert_eq!(format_quantity(0.15), "0.15");
        assert_eq!(format_quantity(1.0), "1");
        assert_eq!(format_quantity(0.001234), "0.001234");
        assert_eq!(format_quantity(0.100000), "0.1");
    }

    #[test]
    fn quantity_formatting_keeps_eight_decimals() {
        // A 1e-5 step size needs five decimals; the old 6-decimal cap was fine
        // here, but sub-satoshi assets need the full eight.
        assert_eq!(format_quantity(0.00012), "0.00012");
        assert_eq!(format_quantity(0.00000001), "0.00000001");
    }

    #[test]
    fn price_formatting_preserves_sub_cent_assets() {
        assert_eq!(format_price(50000.0), "50000");
        assert_eq!(format_price(77_214.24), "77214.24");
        // Two-decimal formatting would have rendered this as "0.00".
        assert_eq!(format_price(0.00001234), "0.00001234");
    }

    // ----- exchange filters -----

    #[test]
    fn symbol_filters_parse_from_exchange_info() {
        // Shape taken from the live BTCUSDC exchangeInfo response.
        let resp = json!({"symbols": [{
            "symbol": "BTCUSDC",
            "baseAsset": "BTC",
            "quoteAsset": "USDC",
            "filters": [
                {"filterType": "PRICE_FILTER", "minPrice": "0.01000000",
                 "maxPrice": "1000000.00000000", "tickSize": "0.01000000"},
                {"filterType": "LOT_SIZE", "minQty": "0.00001000",
                 "maxQty": "9000.00000000", "stepSize": "0.00001000"},
                {"filterType": "NOTIONAL", "minNotional": "5.00000000",
                 "maxNotional": "9000000.00000000"}
            ]
        }]});

        let f = parse_symbol_filters(&resp, "BTCUSDC").unwrap();
        assert_eq!(f.base_asset, "BTC");
        assert_eq!(f.quote_asset, "USDC");
        assert_eq!(f.step_size, 0.00001);
        assert_eq!(f.min_qty, 0.00001);
        assert_eq!(f.tick_size, 0.01);
        assert_eq!(f.min_notional, 5.0);
    }

    #[test]
    fn symbol_filters_accept_the_legacy_min_notional_filter() {
        let resp = json!({"symbols": [{
            "symbol": "BTCUSDT", "baseAsset": "BTC", "quoteAsset": "USDT",
            "filters": [
                {"filterType": "PRICE_FILTER", "tickSize": "0.01000000"},
                {"filterType": "LOT_SIZE", "minQty": "0.00001000",
                 "maxQty": "9000.00000000", "stepSize": "0.00001000"},
                {"filterType": "MIN_NOTIONAL", "minNotional": "10.00000000"}
            ]
        }]});
        assert_eq!(
            parse_symbol_filters(&resp, "BTCUSDT").unwrap().min_notional,
            10.0
        );
    }

    #[test]
    fn symbol_filters_error_when_symbol_absent() {
        let resp = json!({"symbols": []});
        assert!(parse_symbol_filters(&resp, "BTCUSDC").is_err());
    }

    #[test]
    fn fills_carry_commission_and_asset() {
        let resp = json!({
            "executedQty": "0.00012",
            "fills": [{"price": "77214.24", "qty": "0.00012",
                       "commission": "0.00000012", "commissionAsset": "BTC"}]
        });
        let out = parse_order_outcome(&resp, "BTCUSDC", Side::Buy, 0.00012);
        // Binance takes the spot BUY fee out of the base asset, so only
        // 0.00011988 BTC is actually sellable afterwards.
        assert!((out.commission_paid_in("BTC") - 0.00000012).abs() < 1e-12);
    }

    // ----- error detection -----

    #[test]
    fn api_error_body_is_detected() {
        let err = check_api_error(&json!({"code": -1013, "msg": "Filter failure: MIN_NOTIONAL"}));
        assert!(err.unwrap_err().contains("MIN_NOTIONAL"));
    }

    #[test]
    fn successful_body_is_not_an_error() {
        assert!(check_api_error(&json!({"orderId": 123, "status": "FILLED"})).is_ok());
        // code == 0 is Binance's success sentinel on some endpoints.
        assert!(check_api_error(&json!({"code": 0})).is_ok());
    }

    // ----- candles -----

    #[test]
    fn candle_parses_positional_row() {
        let row = json!([
            1_700_000_000_000u64,
            "42000.00",
            "42500.00",
            "41800.00",
            "42300.00",
            "123.45",
            1_700_086_399_999u64,
            "5215000.0",
            900,
            "60.0",
            "2500000.0",
            "0"
        ]);
        let c = parse_candle(&row).expect("row parses");
        assert_eq!(c.open_time, 1_700_000_000_000);
        assert_eq!(c.open, 42_000.0);
        assert_eq!(c.high, 42_500.0);
        assert_eq!(c.low, 41_800.0);
        assert_eq!(c.close, 42_300.0);
        assert_eq!(c.volume, 123.45);
        assert_eq!(c.close_time, 1_700_086_399_999);
    }

    #[test]
    fn truncated_candle_row_is_skipped() {
        assert!(parse_candle(&json!([1_700_000_000_000u64, "42000.00"])).is_none());
    }

    // ----- account -----

    #[test]
    fn account_splits_quote_from_holdings() {
        let resp = json!({"balances": [
            {"asset": "USDC", "free": "19.42", "locked": "0.00"},
            {"asset": "BTC",  "free": "0.001",  "locked": "0.002"},
            {"asset": "ETH",  "free": "0.00",   "locked": "0.00"}
        ]});
        let snap = parse_account(&resp, "USDC").unwrap();
        assert_eq!(snap.free_quote, 19.42);
        // BTC counts free + locked; zero-balance ETH is dropped.
        assert_eq!(
            snap.assets,
            vec![Balance {
                asset: "BTC".into(),
                quantity: 0.003
            }]
        );
    }

    #[test]
    fn account_honours_configured_quote_asset() {
        // The same payload read as a USDT account must not pick up the USDC row
        // as the quote balance.
        let resp = json!({"balances": [
            {"asset": "USDC", "free": "19.42", "locked": "0.00"},
            {"asset": "USDT", "free": "5.00",  "locked": "0.00"}
        ]});
        let snap = parse_account(&resp, "USDT").unwrap();
        assert_eq!(snap.free_quote, 5.00);
        assert_eq!(
            snap.assets,
            vec![Balance {
                asset: "USDC".into(),
                quantity: 19.42
            }]
        );
    }

    #[test]
    fn account_without_balances_is_an_error() {
        assert!(parse_account(&json!({"code": -2015, "msg": "Invalid API-key"}), "USDC").is_err());
    }

    // ----- order outcomes -----

    #[test]
    fn order_outcome_uses_vwap_of_fills() {
        let resp = json!({
            "executedQty": "0.004",
            "cummulativeQuoteQty": "169.00",
            "fills": [
                {"price": "42000.00", "qty": "0.001"},
                {"price": "42500.00", "qty": "0.003"}
            ]
        });
        let out = parse_order_outcome(&resp, "BTCUSDC", Side::Buy, 0.004);
        assert_eq!(out.filled_quantity, 0.004);
        // (42000*0.001 + 42500*0.003) / 0.004 = 42375
        assert!((out.average_price - 42_375.0).abs() < 1e-6);
        assert_eq!(out.fills.len(), 2);
    }

    #[test]
    fn order_outcome_falls_back_to_aggregate_quote_qty() {
        let resp = json!({"executedQty": "0.004", "cummulativeQuoteQty": "169.50"});
        let out = parse_order_outcome(&resp, "BTCUSDC", Side::Buy, 0.004);
        assert!((out.average_price - 42_375.0).abs() < 1e-6);
    }

    #[test]
    fn order_outcome_reports_zero_price_when_unknowable() {
        // Nothing filled: the caller must supply its own reference price rather
        // than trusting a fabricated one.
        let out = parse_order_outcome(&json!({"executedQty": "0"}), "BTCUSDC", Side::Buy, 0.004);
        assert_eq!(out.average_price, 0.0);
        assert_eq!(out.filled_quantity, 0.0);
    }

    #[test]
    fn order_outcome_falls_back_to_requested_quantity() {
        let out = parse_order_outcome(&json!({}), "BTCUSDC", Side::Sell, 0.25);
        assert_eq!(out.filled_quantity, 0.25);
        assert_eq!(out.side, Side::Sell);
    }

    // ----- open orders -----

    #[test]
    fn open_orders_normalise_into_protective_levels() {
        let rows = [
            json!({
                "orderId": 11, "symbol": "BTCUSDC", "side": "SELL",
                "type": "LIMIT_MAKER", "price": "62000.00", "stopPrice": "0.00",
                "origQty": "0.001"
            }),
            json!({
                "orderId": 12, "symbol": "BTCUSDC", "side": "SELL",
                "type": "STOP_LOSS_LIMIT", "price": "56888.00",
                "stopPrice": "57000.00", "origQty": "0.001"
            }),
        ];
        let orders: Vec<OpenOrder> = rows.iter().filter_map(parse_open_order).collect();
        assert_eq!(orders.len(), 2);
        assert_eq!(orders[0].kind, OrderKind::LimitMaker);
        assert_eq!(orders[1].stop_price, 57_000.0);

        let (sl, tp) = protective_levels(&orders);
        assert_eq!(sl, 57_000.0);
        assert_eq!(tp, 62_000.0);
    }

    #[test]
    fn open_order_with_unknown_side_is_skipped() {
        assert!(parse_open_order(&json!({"symbol": "BTCUSDC", "side": "SIDEWAYS"})).is_none());
    }
}
