//! Measures how much a request-counting meter overbills real MCP traffic.
//!
//! The claim this exists to test is that counting HTTP requests is the wrong
//! unit for MCP, because one billable piece of work is not one request. A tool
//! call that asks a question mid-flight is three requests. A durable task
//! polled until it finishes is however many polls the client felt like making.
//! A discovery handshake is several requests that deliver no work at all.
//!
//! So the suite does not argue. It replays scenarios through two meters and
//! prints what each would invoice.
//!
//! # The suite tests this project too
//!
//! Every scenario declares `groundTruthUnits`: what the customer should be
//! charged, with the reasoning written next to it. A meter is wrong when it
//! disagrees with that number, and that includes ours - `ground_truth.rs`
//! fails if terminal-delivery metering ever drifts from the declared answer.
//!
//! Without that the numbers would only be a claim about someone else's
//! software, made by the people selling the alternative. With it, the same
//! file is the regression test and the evidence.

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]

mod meters;
mod report;

pub use meters::{Meter, RequestCounter, RequestPricing, TerminalDelivery};
pub use report::{CategoryTotals, Report, ScenarioOutcome};

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

/// The checked-in scenarios.
pub const SCENARIOS_JSON: &str = include_str!("../scenarios/v1.json");

/// A parsed scenario suite.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Suite {
    /// Bumped when the scenario schema changes shape.
    pub schema_version: u64,
    /// The scenarios, in file order.
    pub scenarios: Vec<Scenario>,
}

impl Suite {
    /// Parse the scenarios shipped with this crate.
    ///
    /// # Panics
    ///
    /// Panics if the checked-in file is not valid for the current schema, which
    /// is a build-time mistake rather than a runtime condition.
    #[must_use]
    pub fn checked_in() -> Self {
        let suite: Self = serde_json::from_str(SCENARIOS_JSON)
            .expect("the checked-in scenario suite must be valid JSON");
        assert_eq!(suite.schema_version, 1, "unsupported scenario schema");
        suite
    }
}

/// One coherent piece of agent work, as the sequence of HTTP exchanges it takes.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Scenario {
    /// Stable identifier, used in output and in test failures.
    pub id: String,
    /// Which overbilling category this belongs to.
    pub category: Category,
    /// One line describing the traffic.
    pub title: String,
    /// Why a request-counting meter gets this wrong.
    pub why: String,
    /// Price for any name without an entry in `named_units`.
    pub flat_units: u64,
    /// Per-name prices, matching `PriceBook::with_name`.
    #[serde(default)]
    pub named_units: BTreeMap<String, u64>,
    /// What the customer should be charged for this scenario.
    pub ground_truth_units: u64,
    /// Why that is the right number. Prose, on purpose: the number is a claim
    /// and this is the argument for it, sitting where it can be disputed.
    pub ground_truth_rationale: String,
    /// The HTTP exchanges, in the order the client made them.
    pub exchanges: Vec<Exchange>,
}

/// One HTTP request and the response it received.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Exchange {
    /// The MCP method, as it appears on the wire.
    pub method: String,
    /// The tool, prompt, or resource name, when the method carries one.
    #[serde(default)]
    pub name: Option<String>,
    /// The JSON-RPC response body.
    pub response: Value,
}

/// The shapes of traffic where request counting and delivery counting diverge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// Connecting: listing tools, reading templates, negotiating.
    Discovery,
    /// One call that takes several round trips to deliver once.
    MultiRoundTrip,
    /// A durable task, polled until it finishes.
    TaskPolling,
    /// The server did not deliver.
    Errors,
    /// The same delivered result observed more than once.
    Retries,
}

impl Category {
    /// Every category, in the order a report prints them.
    pub const ALL: [Self; 5] = [
        Self::Discovery,
        Self::MultiRoundTrip,
        Self::TaskPolling,
        Self::Errors,
        Self::Retries,
    ];

    /// The stable wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discovery => "discovery",
            Self::MultiRoundTrip => "multi_round_trip",
            Self::TaskPolling => "task_polling",
            Self::Errors => "errors",
            Self::Retries => "retries",
        }
    }

    /// A short label for the printed table.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Discovery => "Discovery",
            Self::MultiRoundTrip => "Multi-round-trip",
            Self::TaskPolling => "Task polling",
            Self::Errors => "Errors",
            Self::Retries => "Retries",
        }
    }
}

/// Run every meter over every scenario and collect the result.
#[must_use]
pub fn measure(suite: &Suite, meters: &mut [Box<dyn Meter>]) -> Report {
    let outcomes = suite
        .scenarios
        .iter()
        .map(|scenario| {
            let billed = meters
                .iter_mut()
                .map(|meter| (meter.name().to_owned(), meter.bill(scenario)))
                .collect();
            ScenarioOutcome {
                id: scenario.id.clone(),
                category: scenario.category,
                title: scenario.title.clone(),
                why: scenario.why.clone(),
                requests: scenario.exchanges.len() as u64,
                ground_truth: scenario.ground_truth_units,
                billed,
            }
        })
        .collect();
    Report::new(
        meters.iter().map(|meter| meter.name().to_owned()).collect(),
        outcomes,
    )
}
