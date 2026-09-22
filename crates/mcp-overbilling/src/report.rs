//! Turning measurements into something an operator can read or a script can parse.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde_json::{Value, json};

use crate::Category;

/// What every meter billed for one scenario.
#[derive(Debug, Clone)]
pub struct ScenarioOutcome {
    /// The scenario's stable identifier.
    pub id: String,
    /// Which category it belongs to.
    pub category: Category,
    /// One line describing the traffic.
    pub title: String,
    /// Why request counting gets it wrong.
    pub why: String,
    /// HTTP requests the scenario makes.
    pub requests: u64,
    /// What the customer should be charged.
    pub ground_truth: u64,
    /// Units billed, by meter name.
    pub billed: BTreeMap<String, u64>,
}

/// Per-category sums.
#[derive(Debug, Clone)]
pub struct CategoryTotals {
    /// The category.
    pub category: Category,
    /// Scenarios in it.
    pub scenarios: u64,
    /// HTTP requests across them.
    pub requests: u64,
    /// What should have been charged.
    pub ground_truth: u64,
    /// What each meter charged.
    pub billed: BTreeMap<String, u64>,
}

/// The result of running every meter over every scenario.
#[derive(Debug, Clone)]
pub struct Report {
    /// Meter names, in the order they were run.
    pub meters: Vec<String>,
    /// One entry per scenario.
    pub outcomes: Vec<ScenarioOutcome>,
}

impl Report {
    /// Build a report from raw outcomes.
    #[must_use]
    pub fn new(meters: Vec<String>, outcomes: Vec<ScenarioOutcome>) -> Self {
        Self { meters, outcomes }
    }

    /// Sums for one category, or `None` when no scenario covers it.
    #[must_use]
    pub fn category(&self, category: Category) -> Option<CategoryTotals> {
        let matching: Vec<&ScenarioOutcome> = self
            .outcomes
            .iter()
            .filter(|outcome| outcome.category == category)
            .collect();
        if matching.is_empty() {
            return None;
        }
        let mut billed: BTreeMap<String, u64> = BTreeMap::new();
        for outcome in &matching {
            for (meter, units) in &outcome.billed {
                *billed.entry(meter.clone()).or_default() += units;
            }
        }
        Some(CategoryTotals {
            category,
            scenarios: matching.len() as u64,
            requests: matching.iter().map(|outcome| outcome.requests).sum(),
            ground_truth: matching.iter().map(|outcome| outcome.ground_truth).sum(),
            billed,
        })
    }

    /// Sums across every scenario.
    #[must_use]
    pub fn totals(&self) -> CategoryTotals {
        let mut billed: BTreeMap<String, u64> = BTreeMap::new();
        for outcome in &self.outcomes {
            for (meter, units) in &outcome.billed {
                *billed.entry(meter.clone()).or_default() += units;
            }
        }
        CategoryTotals {
            category: Category::Discovery,
            scenarios: self.outcomes.len() as u64,
            requests: self.outcomes.iter().map(|outcome| outcome.requests).sum(),
            ground_truth: self
                .outcomes
                .iter()
                .map(|outcome| outcome.ground_truth)
                .sum(),
            billed,
        }
    }

    /// Every meter whose total disagrees with ground truth, and by how much.
    ///
    /// A meter that bills less than the truth is as wrong as one that bills
    /// more; it is just wrong in the vendor's favour instead of the
    /// customer's. Both show up here.
    #[must_use]
    pub fn disagreements(&self) -> Vec<(String, String, u64, u64)> {
        let mut found = Vec::new();
        for outcome in &self.outcomes {
            for (meter, units) in &outcome.billed {
                if *units != outcome.ground_truth {
                    found.push((
                        meter.clone(),
                        outcome.id.clone(),
                        outcome.ground_truth,
                        *units,
                    ));
                }
            }
        }
        found
    }

    /// The report as machine-readable JSON.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let categories: Vec<Value> = Category::ALL
            .into_iter()
            .filter_map(|category| self.category(category))
            .map(|totals| {
                json!({
                    "category": totals.category.as_str(),
                    "scenarios": totals.scenarios,
                    "httpRequests": totals.requests,
                    "groundTruthUnits": totals.ground_truth,
                    "billedUnits": totals.billed,
                })
            })
            .collect();
        let totals = self.totals();
        json!({
            "meters": self.meters,
            "scenarios": self.outcomes.iter().map(|outcome| json!({
                "id": outcome.id,
                "category": outcome.category.as_str(),
                "title": outcome.title,
                "why": outcome.why,
                "httpRequests": outcome.requests,
                "groundTruthUnits": outcome.ground_truth,
                "billedUnits": outcome.billed,
            })).collect::<Vec<_>>(),
            "categories": categories,
            "totals": {
                "scenarios": totals.scenarios,
                "httpRequests": totals.requests,
                "groundTruthUnits": totals.ground_truth,
                "billedUnits": totals.billed,
            },
        })
    }

    /// The report as a table for a terminal or a README.
    #[must_use]
    pub fn to_table(&self) -> String {
        let mut out = String::with_capacity(4_096);
        let width = self
            .meters
            .iter()
            .map(|meter| meter.chars().count().max(9))
            .collect::<Vec<_>>();

        out.push_str("Units invoiced for identical MCP traffic\n\n");
        write!(out, "{:<18}{:>6}{:>9}", "Category", "reqs", "correct").ok();
        for (meter, width) in self.meters.iter().zip(&width) {
            write!(out, "{meter:>0$}", width + 2).ok();
        }
        out.push('\n');
        out.push_str(&"-".repeat(33 + width.iter().map(|w| w + 2).sum::<usize>()));
        out.push('\n');

        for category in Category::ALL {
            let Some(totals) = self.category(category) else {
                continue;
            };
            write!(
                out,
                "{:<18}{:>6}{:>9}",
                category.label(),
                totals.requests,
                totals.ground_truth
            )
            .ok();
            for (meter, width) in self.meters.iter().zip(&width) {
                let units = totals.billed.get(meter).copied().unwrap_or_default();
                write!(out, "{units:>0$}", width + 2).ok();
            }
            out.push('\n');
        }

        let totals = self.totals();
        out.push_str(&"-".repeat(33 + width.iter().map(|w| w + 2).sum::<usize>()));
        out.push('\n');
        write!(
            out,
            "{:<18}{:>6}{:>9}",
            "Total", totals.requests, totals.ground_truth
        )
        .ok();
        for (meter, width) in self.meters.iter().zip(&width) {
            let units = totals.billed.get(meter).copied().unwrap_or_default();
            write!(out, "{units:>0$}", width + 2).ok();
        }
        out.push_str("\n\n");

        for meter in &self.meters {
            let billed = totals.billed.get(meter).copied().unwrap_or_default();
            let correct = totals.ground_truth;
            if billed == correct {
                writeln!(out, "{meter}: bills exactly the delivered work.").ok();
                continue;
            }
            // Integer arithmetic throughout. These are invoice figures, and a
            // tool whose whole argument is that other people's billing numbers
            // are wrong should not introduce rounding error of its own.
            let Some(hundredths) = percent_of(billed, correct) else {
                writeln!(out, "{meter}: bills {billed} units for no delivered work.").ok();
                continue;
            };
            let verb = if billed > correct { "over" } else { "under" };
            let difference = percent_of(billed.abs_diff(correct), correct).unwrap_or_default();
            writeln!(
                out,
                "{meter}: {}.{:02}x the correct invoice, {verb}billing by {difference}%.",
                hundredths / 100,
                hundredths % 100
            )
            .ok();
        }
        out
    }
}

/// `value` as a percentage of `whole`, rounded to nearest, or `None` when
/// there is no `whole` to be a percentage of.
fn percent_of(value: u64, whole: u64) -> Option<u64> {
    value
        .saturating_mul(100)
        .saturating_add(whole / 2)
        .checked_div(whole)
}
