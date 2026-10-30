//! Binance's public data archive (`data.binance.vision`): USDⓈ-M futures
//! candles and funding rates for the lab.
//!
//! The archive is static files, readable from places where the futures API
//! itself refuses service. Whole months come as one file each; the current
//! month comes as one file per day, published a day or two late. Funding rates
//! are archived by month only, so the current month has none.

use crate::exchange::Candle;
use std::io::Read;

const ARCHIVE_URL: &str = "https://data.binance.vision/data/futures/um";
const DAY_MS: u64 = 86_400_000;

/// Futures candles for `symbol` with `open_time` in `[start_ms, end_ms)`,
/// oldest first.
pub async fn futures_klines(
    http: &reqwest::Client,
    symbol: &str,
    interval: &str,
    start_ms: u64,
    end_ms: u64,
) -> Result<Vec<Candle>, String> {
    let mut candles = Vec::new();
    for file in files(start_ms, end_ms, now_ms()) {
        let url = match file {
            ArchiveFile::Month(y, m) => format!(
                "{ARCHIVE_URL}/monthly/klines/{symbol}/{interval}/{symbol}-{interval}-{y}-{m:02}.zip"
            ),
            ArchiveFile::Day(y, m, d) => format!(
                "{ARCHIVE_URL}/daily/klines/{symbol}/{interval}/{symbol}-{interval}-{y}-{m:02}-{d:02}.zip"
            ),
        };
        if let Some(csv) = fetch_csv(http, &url).await? {
            candles.extend(parse_klines(&csv));
        }
    }
    candles.retain(|c| c.open_time >= start_ms && c.open_time < end_ms);
    candles.sort_by_key(|c| c.open_time);
    candles.dedup_by_key(|c| c.open_time);
    Ok(candles)
}

/// Funding events `(time_ms, rate)` for `symbol` in `[start_ms, end_ms)`,
/// oldest first. A positive rate means longs pay shorts.
pub async fn funding_rates(
    http: &reqwest::Client,
    symbol: &str,
    start_ms: u64,
    end_ms: u64,
) -> Result<Vec<(u64, f64)>, String> {
    let mut rates = Vec::new();
    for file in files(start_ms, end_ms, now_ms()) {
        let ArchiveFile::Month(y, m) = file else {
            continue;
        };
        let url = format!(
            "{ARCHIVE_URL}/monthly/fundingRate/{symbol}/{symbol}-fundingRate-{y}-{m:02}.zip"
        );
        if let Some(csv) = fetch_csv(http, &url).await? {
            rates.extend(parse_funding(&csv));
        }
    }
    rates.retain(|(t, _)| *t >= start_ms && *t < end_ms);
    rates.sort_by_key(|(t, _)| *t);
    Ok(rates)
}

/// The CSV inside one archive zip, or `None` if the file is not published
/// (before the symbol listed, or a day not yet archived).
async fn fetch_csv(http: &reqwest::Client, url: &str) -> Result<Option<String>, String> {
    let resp = http
        .get(url)
        .send()
        .await
        .map_err(|e| format!("archive {url}: {e}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(format!("archive {url}: HTTP {}", resp.status()));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("archive {url}: {e}"))?;
    unzip_first(&bytes)
        .map(Some)
        .map_err(|e| format!("archive {url}: {e}"))
}

fn unzip_first(bytes: &[u8]) -> Result<String, String> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| e.to_string())?;
    let mut file = zip.by_index(0).map_err(|e| e.to_string())?;
    let mut text = String::new();
    file.read_to_string(&mut text).map_err(|e| e.to_string())?;
    Ok(text)
}

/// `open_time,open,high,low,close,volume,close_time,...`. Older files have no
/// header row; any line that does not parse is skipped.
fn parse_klines(csv: &str) -> Vec<Candle> {
    csv.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(',').collect();
            if f.len() < 7 {
                return None;
            }
            Some(Candle {
                open_time: f[0].trim().parse().ok()?,
                open: f[1].trim().parse().ok()?,
                high: f[2].trim().parse().ok()?,
                low: f[3].trim().parse().ok()?,
                close: f[4].trim().parse().ok()?,
                volume: f[5].trim().parse().ok()?,
                close_time: f[6].trim().parse().ok()?,
            })
        })
        .collect()
}

/// `calc_time,funding_interval_hours,last_funding_rate`.
fn parse_funding(csv: &str) -> Vec<(u64, f64)> {
    csv.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(',').collect();
            if f.len() < 3 {
                return None;
            }
            let rate: f64 = f[2].trim().parse().ok()?;
            let time: u64 = f[0].trim().parse().ok()?;
            rate.is_finite().then_some((time, rate))
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveFile {
    Month(i64, u32),
    Day(i64, u32, u32),
}

/// The files covering `[start_ms, end_ms)`: a monthly file for every month
/// that ended before `now_ms`, daily files for the month still running.
fn files(start_ms: u64, end_ms: u64, now_ms: u64) -> Vec<ArchiveFile> {
    let mut out = Vec::new();
    if start_ms >= end_ms {
        return out;
    }
    let (now_y, now_m, _) = civil(now_ms / DAY_MS);
    let (mut y, mut m, _) = civil(start_ms / DAY_MS);
    let (end_y, end_m, _) = civil((end_ms - 1) / DAY_MS);
    while (y, m) <= (end_y, end_m) {
        if (y, m) < (now_y, now_m) {
            out.push(ArchiveFile::Month(y, m));
        } else {
            let first = days_from_civil(y, m, 1).max(start_ms / DAY_MS);
            let last = ((end_ms - 1) / DAY_MS).min(now_ms / DAY_MS);
            for day in first..=last {
                let (dy, dm, dd) = civil(day);
                if (dy, dm) == (y, m) {
                    out.push(ArchiveFile::Day(dy, dm, dd));
                }
            }
        }
        (y, m) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    }
    out
}

/// Days since the epoch to a proleptic Gregorian `(year, month, day)`
/// (Howard Hinnant's `civil_from_days`).
fn civil(days: u64) -> (i64, u32, u32) {
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> u64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468) as u64
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(u64::MAX, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(y: i64, m: u32, d: u32) -> u64 {
        days_from_civil(y, m, d) * DAY_MS
    }

    #[test]
    fn calendar_round_trips() {
        assert_eq!(civil(0), (1970, 1, 1));
        for (y, m, d) in [(2024, 2, 29), (2026, 10, 30), (2000, 12, 31)] {
            assert_eq!(civil(days_from_civil(y, m, d)), (y, m, d));
        }
    }

    #[test]
    fn past_months_are_monthly_files_and_the_current_month_daily() {
        let now = ms(2026, 10, 30) + 3_600_000;
        let got = files(ms(2026, 8, 15), now, now);
        assert_eq!(got[0], ArchiveFile::Month(2026, 8));
        assert_eq!(got[1], ArchiveFile::Month(2026, 9));
        assert_eq!(got[2], ArchiveFile::Day(2026, 10, 1));
        assert_eq!(got.last(), Some(&ArchiveFile::Day(2026, 10, 30)));
        assert_eq!(got.len(), 2 + 30);
    }

    #[test]
    fn a_range_inside_one_past_month_is_one_file() {
        let now = ms(2026, 10, 30);
        assert_eq!(
            files(ms(2025, 3, 2), ms(2025, 3, 20), now),
            vec![ArchiveFile::Month(2025, 3)]
        );
        assert!(files(5, 5, now).is_empty());
    }

    #[test]
    fn kline_csv_parses_with_or_without_a_header() {
        let csv = "open_time,open,high,low,close,volume,close_time,qv,n,tb,tq,ignore\n\
                   1788220800000,78531.3,78845.5,78130.0,78631.5,4347.329,1788235199999,1,2,3,4,0\n\
                   garbage\n";
        let c = parse_klines(csv);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].open_time, 1_788_220_800_000);
        assert_eq!(c[0].close, 78_631.5);
        assert_eq!(c[0].close_time, 1_788_235_199_999);
    }

    #[test]
    fn funding_csv_parses_rates() {
        let csv = "calc_time,funding_interval_hours,last_funding_rate\n\
                   1788220800005,8,0.00007394\n\
                   1788249600000,8,-0.00010000\n";
        assert_eq!(
            parse_funding(csv),
            vec![
                (1_788_220_800_005, 0.000_073_94),
                (1_788_249_600_000, -0.0001)
            ]
        );
    }
}
