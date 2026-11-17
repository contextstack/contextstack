//! Per-model token rates (`models.json`) and reservation versus settlement pricing (WIRE Request / Response).

use crate::inventory::ModelEntry;
use serde::{Deserialize, Serialize};

/// Per-1M-token list prices from `models.json`. Absent or zero fields fall back to `cost_per_1k`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct ModelRates {
    /// Non-cache input tokens.
    #[serde(default)]
    pub input: f64,
    /// Output tokens.
    #[serde(default)]
    pub output: f64,
    /// Cache read / hit tokens.
    #[serde(default)]
    pub cache_read: f64,
    /// Cache write / creation tokens.
    #[serde(default)]
    pub cache_write: f64,
}

/// Resolved per-1k-token rates used for estimates and settlement.
#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(
    clippy::struct_field_names,
    reason = "per-1k field names match the pricing domain"
)]
pub struct ResolvedRates {
    pub input_per_1k: f64,
    pub output_per_1k: f64,
    pub cache_read_per_1k: f64,
    pub cache_write_per_1k: f64,
}

/// Token counts from a provider usage block.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(
    clippy::struct_field_names,
    reason = "token category names match provider usage blocks"
)]
pub struct TokenBill {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
}

impl ModelEntry {
    /// The rates this entry prices at: explicit `rates` (per 1M) when set, else `cost_per_1k` for every category.
    pub fn resolved_rates(&self) -> ResolvedRates {
        let blend = self.cost_per_1k;
        let per_1k = |per_1m: f64| if per_1m > 0.0 { per_1m / 1000.0 } else { blend };
        let any_explicit = self.rates.input > 0.0
            || self.rates.output > 0.0
            || self.rates.cache_read > 0.0
            || self.rates.cache_write > 0.0;
        if !any_explicit {
            return ResolvedRates {
                input_per_1k: blend,
                output_per_1k: blend,
                cache_read_per_1k: blend,
                cache_write_per_1k: blend,
            };
        }
        ResolvedRates {
            input_per_1k: per_1k(self.rates.input),
            output_per_1k: per_1k(self.rates.output),
            cache_read_per_1k: if self.rates.cache_read > 0.0 {
                per_1k(self.rates.cache_read)
            } else {
                blend
            },
            cache_write_per_1k: if self.rates.cache_write > 0.0 {
                per_1k(self.rates.cache_write)
            } else {
                blend
            },
        }
    }
}

/// Pre-flight hold: the most this call can cost at list price (input estimate plus the output ceiling).
#[allow(clippy::cast_precision_loss)]
pub fn reserve_estimate(
    rates: ResolvedRates,
    body_bytes: usize,
    max_output_tokens: i64,
    default_output: i64,
) -> f64 {
    let out = if max_output_tokens > 0 {
        max_output_tokens
    } else {
        default_output
    };
    let body = i64::try_from(body_bytes).unwrap_or(i64::MAX / 2);
    let input_est = (body + 3) / 4;
    category_usd(rates.input_per_1k, input_est) + category_usd(rates.output_per_1k, out)
}

fn category_usd(rate_per_1k: f64, tokens: i64) -> f64 {
    if rate_per_1k <= 0.0 || tokens <= 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let t = tokens as f64;
    rate_per_1k * t / 1000.0
}

/// List-price USD for actual usage (never under-reported).
pub fn settle_usd(rates: ResolvedRates, bill: TokenBill) -> f64 {
    category_usd(rates.input_per_1k, bill.input_tokens)
        + category_usd(rates.output_per_1k, bill.output_tokens)
        + category_usd(rates.cache_read_per_1k, bill.cache_read_tokens)
        + category_usd(rates.cache_write_per_1k, bill.cache_write_tokens)
}

/// Six-place decimal string for the list-price settlement.
pub fn amount_string(rates: ResolvedRates, bill: TokenBill) -> String {
    let usd = settle_usd(rates, bill);
    if usd <= 0.0 {
        return "0.000000".into();
    }
    format!("{usd:.6}")
}

/// Wallet and settlement charge: provider-reported when present, else list price for api, else 0.
pub fn wallet_usd(class: &str, list_usd: f64, provider: Option<f64>) -> f64 {
    match provider {
        Some(c) if c.is_finite() && c >= 0.0 => c,
        _ => {
            if class == "api" {
                list_usd
            } else {
                0.0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::ModelEntry;

    #[test]
    fn blended_fallback_matches_legacy_formula() {
        let e = ModelEntry {
            cost_per_1k: 0.01,
            ..ModelEntry::default()
        };
        let rates = e.resolved_rates();
        let bill = TokenBill {
            input_tokens: 400,
            output_tokens: 100,
            ..TokenBill::default()
        };
        assert!((settle_usd(rates, bill) - 0.005).abs() < 1e-12);
        let est = reserve_estimate(rates, 400 * 4, 0, 4096);
        let want = 0.01 * (400.0 + 4096.0) / 1000.0;
        assert!((est - want).abs() < 1e-9);
    }

    #[test]
    fn asymmetric_and_cache() {
        let e = ModelEntry {
            cost_per_1k: 0.01,
            rates: ModelRates {
                input: 1.0,
                output: 4.0,
                cache_read: 0.1,
                cache_write: 2.0,
            },
            ..ModelEntry::default()
        };
        let rates = e.resolved_rates();
        let bill = TokenBill {
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 2000,
            cache_write_tokens: 100,
        };
        let want = 0.0034;
        assert!((settle_usd(rates, bill) - want).abs() < 1e-12);
        let est = reserve_estimate(rates, 4000, 500, 4096);
        let want_est = 0.003;
        assert!((est - want_est).abs() < 1e-12);
    }
}
