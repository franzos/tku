use std::fs;
use std::time::SystemTime;

use anyhow::Result;
use directories::ProjectDirs;

const CACHE_TTL_SECS: u64 = 7 * 24 * 60 * 60;

pub struct ExchangeRate {
    pub symbol: String,
    pub rate: f64,
    pub code: String,
}

impl ExchangeRate {
    pub fn usd() -> Self {
        Self {
            symbol: "$".to_string(),
            rate: 1.0,
            code: "USD".to_string(),
        }
    }

    pub fn convert(&self, usd: f64) -> f64 {
        usd * self.rate
    }

    pub fn format_cost(&self, cost: Option<f64>) -> String {
        match cost {
            Some(c) => format!("{}{:.2}", self.symbol, self.convert(c)),
            None => "N/A".to_string(),
        }
    }
}

fn currency_symbol(code: &str) -> &str {
    match code {
        "USD" => "$",
        "EUR" => "€",
        "GBP" => "£",
        "JPY" => "¥",
        "CNY" => "¥",
        "KRW" => "₩",
        "INR" => "₹",
        "BRL" => "R$",
        "CHF" => "CHF ",
        "CAD" => "CA$",
        "AUD" => "A$",
        "SEK" => "kr ",
        "NOK" => "kr ",
        "DKK" => "kr ",
        "PLN" => "zł",
        "CZK" => "Kč ",
        "TRY" => "₺",
        "THB" => "฿",
        "MXN" => "MX$",
        "ZAR" => "R ",
        _ => "",
    }
}

fn cache_path() -> Option<std::path::PathBuf> {
    ProjectDirs::from("", "", "tku").map(|d| d.cache_dir().join("exchange.json"))
}

fn cache_is_fresh(path: &std::path::PathBuf) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .map(|d| d.as_secs() < CACHE_TTL_SECS)
        .unwrap_or(false)
}

#[derive(serde::Deserialize)]
struct FrankfurterResponse {
    rates: std::collections::HashMap<String, f64>,
}

fn fetch_rate(currency: &str) -> Result<f64> {
    let url = format!(
        "https://api.frankfurter.dev/v1/latest?base=USD&symbols={}",
        currency
    );
    let body = ureq::get(&url).call()?.body_mut().read_to_string()?;
    let resp: FrankfurterResponse = serde_json::from_str(&body)?;
    resp.rates
        .get(currency)
        .copied()
        .ok_or_else(|| anyhow::anyhow!("Currency {} not found in response", currency))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CachedRate {
    code: String,
    rate: f64,
}

fn load_cached_rate(currency: &str, require_fresh: bool) -> Option<f64> {
    let path = cache_path()?;
    if require_fresh && !cache_is_fresh(&path) {
        return None;
    }
    let data = fs::read_to_string(&path).ok()?;
    let cached: CachedRate = serde_json::from_str(&data).ok()?;
    if cached.code == currency {
        Some(cached.rate)
    } else {
        None
    }
}

fn save_cached_rate(currency: &str, rate: f64) {
    let Some(path) = cache_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let cached = CachedRate {
        code: currency.to_string(),
        rate,
    };
    let _ = fs::write(&path, serde_json::to_string(&cached).unwrap_or_default());
}

pub fn load_exchange_rate(currency: &str, offline: bool) -> ExchangeRate {
    let code = currency.to_uppercase();

    if code == "USD" {
        return ExchangeRate::usd();
    }

    let sym = currency_symbol(&code);
    let symbol = if sym.is_empty() {
        format!("{} ", code)
    } else {
        sym.to_string()
    };

    // Try fresh cache first
    if let Some(rate) = load_cached_rate(&code, true) {
        return ExchangeRate { symbol, rate, code };
    }

    if offline {
        // Offline: accept stale cache
        if let Some(rate) = load_cached_rate(&code, false) {
            eprintln!("Warning: using stale exchange rate for {}", code);
            return ExchangeRate { symbol, rate, code };
        }
        eprintln!(
            "Warning: no cached exchange rate for {}, falling back to USD",
            code
        );
        return ExchangeRate::usd();
    }

    // Fetch live
    match fetch_rate(&code) {
        Ok(rate) => {
            save_cached_rate(&code, rate);
            ExchangeRate { symbol, rate, code }
        }
        Err(e) => {
            eprintln!("Warning: failed to fetch exchange rate for {}: {}", code, e);
            // Try stale cache before giving up
            if let Some(rate) = load_cached_rate(&code, false) {
                eprintln!("Using cached rate for {}", code);
                ExchangeRate { symbol, rate, code }
            } else {
                eprintln!("Falling back to USD");
                ExchangeRate::usd()
            }
        }
    }
}
