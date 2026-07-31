//! The price book: how many billable units a delivered call is worth.
//!
//! Units are integers, not currency. Converting units to money is the billing
//! provider's job (Stripe meters, Paddle, an invoice line), and keeping money out
//! of this crate keeps rounding, currency, and tax out of the attribution engine
//! where they would only cause trouble.
//!
//! Pricing is keyed on the value of `Mcp-Name`, which covers a tool name, a
//! prompt name, or a resource URI depending on the method. One map therefore
//! prices all three, which is what operators actually want: an expensive tool and
//! an expensive resource are the same kind of problem.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::Method;

const fn default_units() -> u64 {
    1
}

/// Per-name unit pricing with a fallback.
///
/// `BTreeMap` rather than `HashMap` so a serialized price book round trips in a
/// stable order, which makes config diffs readable and tests deterministic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceBook {
    /// Charged for a delivered call with no entry in [`Self::names`].
    #[serde(default = "default_units")]
    pub default_units: u64,

    /// Per-name overrides, keyed on the decoded `Mcp-Name` value.
    ///
    /// A value of `0` makes that name free, which is the supported way to expose
    /// a loss-leader tool without carving it out of the billing path.
    #[serde(default)]
    pub names: BTreeMap<String, u64>,

    /// Per-method overrides, applied when no name matched.
    ///
    /// Mainly useful for pricing `resources/read` differently from `tools/call`
    /// across the board.
    #[serde(default)]
    pub methods: BTreeMap<String, u64>,
}

impl Default for PriceBook {
    fn default() -> Self {
        Self {
            default_units: default_units(),
            names: BTreeMap::new(),
            methods: BTreeMap::new(),
        }
    }
}

impl PriceBook {
    /// A price book charging `units` for everything.
    #[must_use]
    pub fn flat(units: u64) -> Self {
        Self {
            default_units: units,
            ..Self::default()
        }
    }

    /// Set the price of one name. Chainable, for building books in tests and config.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>, units: u64) -> Self {
        self.names.insert(name.into(), units);
        self
    }

    /// Set the price of one method.
    #[must_use]
    pub fn with_method(mut self, method: &Method, units: u64) -> Self {
        self.methods.insert(method.as_str().to_owned(), units);
        self
    }

    /// Units owed for a delivered call.
    ///
    /// Resolution order is most specific first: the name, then the method, then
    /// the default. `name` must already be decoded from the `Mcp-Name` sentinel
    /// form (see [`crate::name::decode`]); passing the raw header value here
    /// silently prices every non-ASCII-named tool at the default.
    #[must_use]
    pub fn units_for(&self, method: &Method, name: Option<&str>) -> u64 {
        if let Some(units) = name.and_then(|n| self.names.get(n)) {
            return *units;
        }
        if let Some(units) = self.methods.get(method.as_str()) {
            return *units;
        }
        self.default_units
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_one_unit_per_delivered_call() {
        let book = PriceBook::default();
        assert_eq!(book.units_for(&Method::ToolsCall, Some("anything")), 1);
        assert_eq!(book.units_for(&Method::ToolsCall, None), 1);
    }

    #[test]
    fn name_beats_method_beats_default() {
        let book = PriceBook::flat(1)
            .with_method(&Method::ToolsCall, 5)
            .with_name("expensive_tool", 100);

        assert_eq!(
            book.units_for(&Method::ToolsCall, Some("expensive_tool")),
            100
        );
        assert_eq!(book.units_for(&Method::ToolsCall, Some("other_tool")), 5);
        assert_eq!(book.units_for(&Method::ToolsCall, None), 5);
        assert_eq!(book.units_for(&Method::ResourcesRead, Some("other")), 1);
    }

    #[test]
    fn zero_units_makes_a_name_free() {
        let book = PriceBook::flat(10).with_name("free_probe", 0);
        assert_eq!(book.units_for(&Method::ToolsCall, Some("free_probe")), 0);
        assert_eq!(book.units_for(&Method::ToolsCall, Some("paid")), 10);
    }

    #[test]
    fn one_book_prices_tools_prompts_and_resources_alike() {
        // Mcp-Name carries a tool name, a prompt name, or a resource URI, so a
        // single name map covers all three.
        let book = PriceBook::flat(1)
            .with_name("get_weather", 3)
            .with_name("file:///big/dataset.csv", 50);

        assert_eq!(book.units_for(&Method::ToolsCall, Some("get_weather")), 3);
        assert_eq!(
            book.units_for(&Method::ResourcesRead, Some("file:///big/dataset.csv")),
            50
        );
    }

    #[test]
    fn round_trips_through_json_in_stable_order() {
        let book = PriceBook::flat(2)
            .with_name("zebra", 9)
            .with_name("alpha", 1)
            .with_method(&Method::ResourcesRead, 4);

        let encoded = serde_json::to_string(&book).expect("serializes");
        let decoded: PriceBook = serde_json::from_str(&encoded).expect("deserializes");
        assert_eq!(book, decoded);
        // BTreeMap ordering keeps config diffs readable.
        assert!(
            encoded.find("alpha").unwrap() < encoded.find("zebra").unwrap(),
            "names should serialize in sorted order: {encoded}"
        );
    }

    #[test]
    fn a_minimal_config_deserializes() {
        let book: PriceBook = serde_json::from_str("{}").expect("empty config is valid");
        assert_eq!(book, PriceBook::default());

        let book: PriceBook =
            serde_json::from_str(r#"{"default_units":7}"#).expect("partial config is valid");
        assert_eq!(book.default_units, 7);
        assert!(book.names.is_empty());
    }
}
