//! Binance USDⓈ-M futures REST adapter.
//!
//! Same signing and number encoding as spot, different host and endpoints.
//! Stops and targets are conditional orders, which Binance serves from its
//! Algo Order endpoints (`/fapi/v1/algoOrder`); the plain order endpoint
//! rejects them with `-4120`.

use super::binance::{
    check_api_error, format_decimal, parse_candle, parse_str_f64, parse_str_f64_or_zero, sign,
};
use super::{
    Candle, FuturesAccount, FuturesExchange, FuturesPosition, OrderOutcome, Side, SymbolFilters,
    Transfer, TriggerKind, TriggerOrder,
};
use std::collections::HashMap;
use std::sync::Mutex;

pub const PRODUCTION_URL: &str = "https://fapi.binance.com";

pub struct BinanceFuturesClient {
    base_url: String,
    api_key: String,
    api_secret: String,
    http: reqwest::Client,
    /// Trading rules are effectively static; fetched once per symbol.
    filter_cache: Mutex<HashMap<String, SymbolFilters>>,
}

impl BinanceFuturesClient {
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

    fn timestamp_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("Time went backwards")
            .as_millis() as u64
    }

    async fn public_get(&self, path_and_query: &str) -> Result<serde_json::Value, String> {
        let resp: serde_json::Value = self
            .http
            .get(format!("{}{path_and_query}", self.base_url))
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?
            .json()
            .await
            .map_err(|e| format!("JSON parse error: {e}"))?;
        check_api_error(&resp)?;
        Ok(resp)
    }

    /// A signed request. `params` excludes the timestamp and signature, which
    /// are appended here. GET and DELETE carry them in the URL, POST in the
    /// form body.
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

impl FuturesExchange for BinanceFuturesClient {
    async fn klines(
        &self,
        symbol: &str,
        interval: &str,
        limit: u32,
    ) -> Result<Vec<Candle>, String> {
        let resp = self
            .public_get(&format!(
                "/fapi/v1/klines?symbol={symbol}&interval={interval}&limit={limit}"
            ))
            .await?;
        let rows = resp
            .as_array()
            .ok_or_else(|| format!("klines request failed: {resp}"))?;
        Ok(rows.iter().filter_map(parse_candle).collect())
    }

    async fn filters(&self, symbol: &str) -> Result<SymbolFilters, String> {
        if let Some(cached) = self
            .filter_cache
            .lock()
            .ok()
            .and_then(|c| c.get(symbol).cloned())
        {
            return Ok(cached);
        }
        let resp = self.public_get("/fapi/v1/exchangeInfo").await?;
        let filters = parse_futures_filters(&resp, symbol)?;
        if let Ok(mut cache) = self.filter_cache.lock() {
            cache.insert(symbol.to_string(), filters.clone());
        }
        Ok(filters)
    }

    async fn account(&self) -> Result<FuturesAccount, String> {
        let resp = self
            .signed(reqwest::Method::GET, "/fapi/v3/account", "")
            .await?;
        parse_account(&resp)
    }

    async fn positions(&self) -> Result<Vec<FuturesPosition>, String> {
        let resp = self
            .signed(reqwest::Method::GET, "/fapi/v3/positionRisk", "")
            .await?;
        parse_positions(&resp)
    }

    async fn trigger_orders(&self, symbol: &str) -> Result<Vec<TriggerOrder>, String> {
        let resp = self
            .signed(
                reqwest::Method::GET,
                "/fapi/v1/openAlgoOrders",
                &format!("symbol={symbol}"),
            )
            .await?;
        parse_trigger_orders(&resp)
    }

    async fn market_order(
        &self,
        symbol: &str,
        side: Side,
        quantity: f64,
        reduce_only: bool,
    ) -> Result<OrderOutcome, String> {
        let params = format!(
            "symbol={symbol}&side={}&type=MARKET&quantity={}&reduceOnly={reduce_only}\
             &newOrderRespType=RESULT",
            side.as_str(),
            format_decimal(quantity),
        );
        let resp = self
            .signed(reqwest::Method::POST, "/fapi/v1/order", &params)
            .await?;
        parse_market_order(&resp, symbol, side)
    }

    async fn place_trigger(
        &self,
        symbol: &str,
        side: Side,
        kind: TriggerKind,
        trigger_price: f64,
    ) -> Result<u64, String> {
        let order_type = match kind {
            TriggerKind::Stop => "STOP_MARKET",
            TriggerKind::TakeProfit => "TAKE_PROFIT_MARKET",
        };
        // Mark price, not last trade, so one wick on thin liquidity does not
        // fire the stop. Price protection stays off: it holds the trigger back
        // exactly when mark and last diverge, which is what a crash does.
        let params = format!(
            "algoType=CONDITIONAL&symbol={symbol}&side={}&type={order_type}&triggerPrice={}\
             &closePosition=true&workingType=MARK_PRICE",
            side.as_str(),
            format_decimal(trigger_price),
        );
        let resp = self
            .signed(reqwest::Method::POST, "/fapi/v1/algoOrder", &params)
            .await?;
        resp["algoId"]
            .as_u64()
            .ok_or_else(|| format!("no algoId in conditional order response: {resp}"))
    }

    async fn cancel_all(&self, symbol: &str) -> Result<(), String> {
        let params = format!("symbol={symbol}");
        self.signed(reqwest::Method::DELETE, "/fapi/v1/algoOpenOrders", &params)
            .await?;
        self.signed(reqwest::Method::DELETE, "/fapi/v1/allOpenOrders", &params)
            .await?;
        Ok(())
    }

    async fn set_leverage(&self, symbol: &str, leverage: u32) -> Result<(), String> {
        self.signed(
            reqwest::Method::POST,
            "/fapi/v1/leverage",
            &format!("symbol={symbol}&leverage={leverage}"),
        )
        .await
        .map(|_| ())
    }

    async fn hedge_mode(&self) -> Result<bool, String> {
        let resp = self
            .signed(reqwest::Method::GET, "/fapi/v1/positionSide/dual", "")
            .await?;
        resp["dualSidePosition"]
            .as_bool()
            .ok_or_else(|| format!("unexpected position mode response: {resp}"))
    }

    async fn transfers(&self, since_ms: u64) -> Result<Vec<Transfer>, String> {
        let resp = self
            .signed(
                reqwest::Method::GET,
                "/fapi/v1/income",
                &format!("incomeType=TRANSFER&startTime={since_ms}&limit=1000"),
            )
            .await?;
        parse_transfers(&resp)
    }
}

/// Futures filters differ from spot: market orders have their own lot size,
/// and the minimum order value sits under `MIN_NOTIONAL.notional`.
fn parse_futures_filters(resp: &serde_json::Value, symbol: &str) -> Result<SymbolFilters, String> {
    let entry = resp["symbols"]
        .as_array()
        .and_then(|symbols| {
            symbols
                .iter()
                .find(|s| s["symbol"].as_str() == Some(symbol))
        })
        .ok_or_else(|| format!("futures exchangeInfo has no entry for {symbol}"))?;
    if entry["contractType"].as_str() != Some("PERPETUAL") {
        return Err(format!("{symbol} is not a perpetual contract"));
    }
    if entry["status"].as_str() != Some("TRADING") {
        return Err(format!("{symbol} is not trading"));
    }
    let find = |kind: &str| {
        entry["filters"].as_array().and_then(|filters| {
            filters
                .iter()
                .find(|f| f["filterType"].as_str() == Some(kind))
                .cloned()
        })
    };
    let lot = find("LOT_SIZE").ok_or_else(|| format!("{symbol}: no LOT_SIZE filter"))?;
    let price = find("PRICE_FILTER").ok_or_else(|| format!("{symbol}: no PRICE_FILTER"))?;
    let market = find("MARKET_LOT_SIZE");
    let market_field = |key: &str| market.as_ref().map(|m| parse_str_f64_or_zero(&m[key]));
    let step = parse_str_f64_or_zero(&lot["stepSize"]).max(market_field("stepSize").unwrap_or(0.0));
    let min_qty = parse_str_f64_or_zero(&lot["minQty"]).max(market_field("minQty").unwrap_or(0.0));
    let lot_max = parse_str_f64_or_zero(&lot["maxQty"]);
    let max_qty = market_field("maxQty")
        .filter(|m| *m > 0.0)
        .map_or(lot_max, |m| m.min(lot_max));
    Ok(SymbolFilters {
        symbol: symbol.to_string(),
        base_asset: entry["baseAsset"].as_str().unwrap_or_default().to_string(),
        quote_asset: entry["marginAsset"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        step_size: step,
        min_qty,
        max_qty,
        tick_size: parse_str_f64_or_zero(&price["tickSize"]),
        min_notional: find("MIN_NOTIONAL")
            .map(|n| parse_str_f64_or_zero(&n["notional"]))
            .unwrap_or(0.0),
    })
}

fn parse_account(resp: &serde_json::Value) -> Result<FuturesAccount, String> {
    Ok(FuturesAccount {
        equity: parse_str_f64(&resp["totalMarginBalance"])
            .ok_or_else(|| format!("no totalMarginBalance in account: {resp}"))?,
        available: parse_str_f64(&resp["availableBalance"])
            .ok_or_else(|| format!("no availableBalance in account: {resp}"))?,
    })
}

fn parse_positions(resp: &serde_json::Value) -> Result<Vec<FuturesPosition>, String> {
    let rows = resp
        .as_array()
        .ok_or_else(|| format!("unexpected positionRisk response: {resp}"))?;
    Ok(rows
        .iter()
        .filter_map(|p| {
            let amount = parse_str_f64(&p["positionAmt"])?;
            (amount != 0.0).then(|| FuturesPosition {
                symbol: p["symbol"].as_str().unwrap_or_default().to_string(),
                amount,
                entry_price: parse_str_f64_or_zero(&p["entryPrice"]),
                mark_price: parse_str_f64_or_zero(&p["markPrice"]),
                liquidation_price: parse_str_f64_or_zero(&p["liquidationPrice"]),
            })
        })
        .collect())
}

fn parse_trigger_orders(resp: &serde_json::Value) -> Result<Vec<TriggerOrder>, String> {
    // A list, or a list under `orders` depending on the API version.
    let rows = resp
        .as_array()
        .or_else(|| resp["orders"].as_array())
        .ok_or_else(|| format!("unexpected open algo orders response: {resp}"))?;
    Ok(rows
        .iter()
        .filter_map(|o| {
            let side = match o["side"].as_str()? {
                "BUY" => Side::Buy,
                "SELL" => Side::Sell,
                _ => return None,
            };
            let kind = match o["orderType"].as_str().or_else(|| o["type"].as_str()) {
                Some("STOP_MARKET") => Some(TriggerKind::Stop),
                Some("TAKE_PROFIT_MARKET") => Some(TriggerKind::TakeProfit),
                _ => None,
            };
            Some(TriggerOrder {
                id: o["algoId"].as_u64()?,
                kind,
                side,
                trigger_price: parse_str_f64_or_zero(&o["triggerPrice"]),
            })
        })
        .collect())
}

fn parse_market_order(
    resp: &serde_json::Value,
    symbol: &str,
    side: Side,
) -> Result<OrderOutcome, String> {
    let filled = parse_str_f64_or_zero(&resp["executedQty"]);
    if filled <= 0.0 {
        return Err(format!("market order for {symbol} did not fill: {resp}"));
    }
    Ok(OrderOutcome {
        symbol: symbol.to_string(),
        side,
        filled_quantity: filled,
        average_price: parse_str_f64_or_zero(&resp["avgPrice"]),
        fills: Vec::new(),
    })
}

/// Income history rows of type `TRANSFER`: positive into the futures wallet,
/// negative out of it.
fn parse_transfers(resp: &serde_json::Value) -> Result<Vec<Transfer>, String> {
    let rows = resp
        .as_array()
        .ok_or_else(|| format!("unexpected income response: {resp}"))?;
    Ok(rows
        .iter()
        .filter(|r| r["incomeType"].as_str() == Some("TRANSFER"))
        .filter_map(|r| {
            Some(Transfer {
                id: format!(
                    "futures-transfer:{}",
                    r["tranId"]
                        .as_u64()
                        .map(|id| id.to_string())
                        .or_else(|| r["tranId"].as_str().map(str::to_string))?
                ),
                asset: r["asset"].as_str()?.to_string(),
                amount: parse_str_f64(&r["income"])?,
                time_ms: r["time"].as_u64()?,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn exchange_info() -> serde_json::Value {
        json!({"symbols": [{
            "symbol": "BTCUSDC", "contractType": "PERPETUAL", "status": "TRADING",
            "baseAsset": "BTC", "marginAsset": "USDC",
            "filters": [
                {"filterType": "PRICE_FILTER", "tickSize": "0.10"},
                {"filterType": "LOT_SIZE", "stepSize": "0.001", "minQty": "0.001", "maxQty": "1000"},
                {"filterType": "MARKET_LOT_SIZE", "stepSize": "0.001", "minQty": "0.001", "maxQty": "120"},
                {"filterType": "MIN_NOTIONAL", "notional": "50"}
            ]
        }]})
    }

    #[test]
    fn futures_filters_use_the_market_lot_and_notional() {
        let f = parse_futures_filters(&exchange_info(), "BTCUSDC").unwrap();
        assert_eq!(f.step_size, 0.001);
        assert_eq!(f.min_qty, 0.001);
        assert_eq!(
            f.max_qty, 120.0,
            "market orders cap lower than limit orders"
        );
        assert_eq!(f.tick_size, 0.1);
        assert_eq!(f.min_notional, 50.0);
        assert_eq!(f.quote_asset, "USDC");
        assert!(parse_futures_filters(&exchange_info(), "ETHUSDC").is_err());
    }

    #[test]
    fn a_delivery_contract_is_refused() {
        let mut info = exchange_info();
        info["symbols"][0]["contractType"] = json!("CURRENT_QUARTER");
        assert!(parse_futures_filters(&info, "BTCUSDC").is_err());
    }

    #[test]
    fn account_reads_margin_balance_and_available() {
        let a = parse_account(&json!({
            "totalMarginBalance": "23.32", "availableBalance": "20.10", "totalWalletBalance": "23.30"
        }))
        .unwrap();
        assert_eq!(
            a,
            FuturesAccount {
                equity: 23.32,
                available: 20.10
            }
        );
        assert!(parse_account(&json!({})).is_err());
    }

    #[test]
    fn only_open_positions_are_returned_with_their_side() {
        let resp = json!([
            {"symbol": "ETHUSDC", "positionAmt": "-0.010", "entryPrice": "2243.1",
             "markPrice": "2240.0", "liquidationPrice": "2900.5"},
            {"symbol": "BTCUSDC", "positionAmt": "0.000", "entryPrice": "0.0",
             "markPrice": "78000", "liquidationPrice": "0"}
        ]);
        let p = parse_positions(&resp).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].symbol, "ETHUSDC");
        assert!(p[0].is_short());
        assert_eq!(p[0].liquidation_price, 2_900.5);
    }

    #[test]
    fn conditional_orders_are_classified() {
        let resp = json!([
            {"algoId": 1, "orderType": "STOP_MARKET", "side": "SELL", "triggerPrice": "76000.0"},
            {"algoId": 2, "orderType": "TAKE_PROFIT_MARKET", "side": "SELL", "triggerPrice": "82000.0"},
            {"algoId": 3, "orderType": "TRAILING_STOP_MARKET", "side": "SELL", "triggerPrice": "0"}
        ]);
        let o = parse_trigger_orders(&resp).unwrap();
        assert_eq!(o.len(), 3);
        assert_eq!(o[0].kind, Some(TriggerKind::Stop));
        assert_eq!(o[0].trigger_price, 76_000.0);
        assert_eq!(o[1].kind, Some(TriggerKind::TakeProfit));
        assert_eq!(o[2].kind, None);
        let wrapped = parse_trigger_orders(&json!({"orders": resp})).unwrap();
        assert_eq!(wrapped, o);
    }

    #[test]
    fn a_market_order_reports_its_average_fill() {
        let o = parse_market_order(
            &json!({"status": "FILLED", "executedQty": "0.010", "avgPrice": "2243.50"}),
            "ETHUSDC",
            Side::Buy,
        )
        .unwrap();
        assert_eq!(o.filled_quantity, 0.01);
        assert_eq!(o.average_price, 2_243.5);
        assert!(parse_market_order(&json!({"executedQty": "0"}), "ETHUSDC", Side::Buy).is_err());
    }

    #[test]
    fn transfers_come_from_income_history() {
        let resp = json!([
            {"symbol": "", "incomeType": "TRANSFER", "income": "23.36", "asset": "USDC",
             "time": 1_793_340_000_000_u64, "tranId": 9_001_u64},
            {"symbol": "ETHUSDC", "incomeType": "FUNDING_FEE", "income": "-0.01", "asset": "USDC",
             "time": 1_793_340_100_000_u64, "tranId": 9_002_u64},
            {"symbol": "", "incomeType": "TRANSFER", "income": "-5", "asset": "USDC",
             "time": 1_793_340_200_000_u64, "tranId": "9003"}
        ]);
        let t = parse_transfers(&resp).unwrap();
        assert_eq!(t.len(), 2, "funding is P&L, not a transfer");
        assert_eq!(t[0].id, "futures-transfer:9001");
        assert_eq!(t[0].amount, 23.36);
        assert_eq!(t[1].id, "futures-transfer:9003");
        assert_eq!(t[1].amount, -5.0);
    }
}

/// Against Binance's futures testnet; needs the network, so run with
/// `cargo test -- --ignored`.
#[cfg(test)]
mod testnet {
    use super::*;

    #[tokio::test]
    #[ignore]
    async fn public_endpoints_parse() {
        let c = BinanceFuturesClient::new("https://testnet.binancefuture.com", "", "");
        let candles = c.klines("ETHUSDC", "15m", 5).await.unwrap();
        assert_eq!(candles.len(), 5);
        assert!(candles.windows(2).all(|w| w[0].open_time < w[1].open_time));
        let f = c.filters("ETHUSDC").await.unwrap();
        assert!(
            f.step_size > 0.0 && f.tick_size > 0.0 && f.min_notional > 0.0,
            "{f:?}"
        );
    }
}
