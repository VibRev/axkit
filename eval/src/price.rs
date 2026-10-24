//! Model prices, so cost is computed from tokens rather than trusted from the
//! stream. A model missing from the table has an unknown price; nothing is
//! guessed.

use crate::metrics::Tokens;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// The table shipped with the runner; see its header for the sources.
const BUILTIN: &str = include_str!("../prices.toml");

/// Dollars per million tokens, by kind.
#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    pub cache_read: f64,
}

impl Price {
    #[must_use]
    pub fn cost(self, t: Tokens) -> f64 {
        let per = |n: u64, rate: f64| n as f64 / 1_000_000.0 * rate;
        per(t.input, self.input)
            + per(t.output, self.output)
            + per(t.cache_write_5m, self.cache_write_5m)
            + per(t.cache_write_1h, self.cache_write_1h)
            + per(t.cache_read, self.cache_read)
    }
}

#[derive(Debug, Deserialize)]
struct Row {
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(flatten)]
    price: Price,
}

#[derive(Debug, Deserialize)]
struct Table {
    models: BTreeMap<String, Row>,
}

#[derive(Debug)]
pub struct Prices {
    models: BTreeMap<String, Row>,
}

impl Prices {
    /// The built-in table.
    ///
    /// # Panics
    ///
    /// Never in a release: the built-in table is checked by a unit test.
    #[must_use]
    pub fn builtin() -> Prices {
        Prices::parse(BUILTIN).expect("the built-in prices.toml parses (unit-tested)")
    }

    /// The built-in table with `path`'s rows layered over it.
    ///
    /// # Errors
    ///
    /// `path` cannot be read or is not a price table.
    pub fn with_overrides(path: &Path) -> Result<Prices> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut prices = Prices::builtin();
        prices.models.extend(Prices::parse(&text).with_context(|| format!("parsing {}", path.display()))?.models);
        Ok(prices)
    }

    fn parse(text: &str) -> Result<Prices> {
        let table: Table = toml::from_str(text)?;
        Ok(Prices { models: table.models })
    }

    /// The price of `model`: an exact id, else the longest alias it contains.
    /// `None` when the model is unknown.
    #[must_use]
    pub fn get(&self, model: &str) -> Option<Price> {
        if let Some(row) = self.models.get(model) {
            return Some(row.price);
        }
        let name = model.to_ascii_lowercase();
        self.models
            .values()
            .flat_map(|row| row.aliases.iter().map(move |alias| (alias, row)))
            .filter(|(alias, _)| name.contains(alias.as_str()))
            .max_by_key(|(alias, _)| alias.len())
            .map(|(_, row)| row.price)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builtin_table_parses_and_matches_ids_and_aliases() {
        let prices = Prices::builtin();
        let haiku = prices.get("claude-haiku-5-5").unwrap();
        assert!((haiku.input - 0.10).abs() < 1e-12);
        assert_eq!(prices.get("haiku"), Some(haiku));
        assert_eq!(prices.get("claude-haiku-4-5").map(|p| p.input), Some(1.0), "the longer alias wins");
        assert_eq!(prices.get("claude-opus-5-5-fast").map(|p| p.input), Some(4.0));
    }

    #[test]
    fn unknown_models_have_no_price() {
        let prices = Prices::builtin();
        assert_eq!(prices.get("gpt-6-astra"), None);
        assert_eq!(prices.get("grok-4.7"), None);
    }

    #[test]
    fn cost_prices_each_kind_at_its_rate() {
        let p = Price { input: 1.0, output: 5.0, cache_write_5m: 1.25, cache_write_1h: 2.0, cache_read: 0.1 };
        let t = Tokens {
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 1_000_000,
            cache_write_5m: 1_000_000,
            cache_write_1h: 1_000_000,
            reasoning: 9,
        };
        assert!((p.cost(t) - (1.0 + 5.0 + 0.1 + 1.25 + 2.0)).abs() < 1e-9);
    }

    #[test]
    fn an_override_file_adds_or_replaces_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prices.toml");
        std::fs::write(
            &path,
            "[models.grok-4-7]\naliases=[\"grok-4.7\"]\ninput=1\noutput=2\ncache_write_5m=0\ncache_write_1h=0\ncache_read=0.5\n",
        )
        .unwrap();
        let prices = Prices::with_overrides(&path).unwrap();
        assert_eq!(prices.get("grok-4.7").map(|p| p.output), Some(2.0));
        assert!(prices.get("haiku").is_some(), "built-in rows stay");
    }
}
