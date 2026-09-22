//! The meters under test.

use std::collections::{HashMap, HashSet};

use mcp_usage_core::{Call, Charge, Method, PriceBook, ResultType, decide_with_task_origin, peek};

use crate::Scenario;

/// A metering strategy.
///
/// Implement this to put another vendor's rules next to these. `bill` sees one
/// whole scenario, in order, because that is the only way to represent a meter
/// with memory: deduplication and task attribution both need to remember what
/// earlier exchanges said.
pub trait Meter {
    /// How this meter is named in the report.
    fn name(&self) -> &str;
    /// Total units this meter would invoice for the scenario.
    fn bill(&mut self, scenario: &Scenario) -> u64;
}

/// How a request counter turns a request into units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestPricing {
    /// One flat rate per request, whatever was called.
    ///
    /// What an HTTP middleware or API gateway can do: it sees a request
    /// arrive and has no idea which tool it was for.
    Flat,
    /// The called tool's own price, per request.
    ///
    /// The best a request counter can do - it reads `Mcp-Name` and applies the
    /// customer's price book, and is only blind to what came *back*. This is
    /// the steelman, and the headline comparison, because beating the weaker
    /// version would prove nothing.
    Named,
}

/// Counts HTTP requests.
///
/// Not a strawman: this is what you get by pointing a general-purpose usage
/// meter at an MCP server, which is what the metering vendors in this space
/// instruct you to do. It is wrong for one reason only - it decides at the
/// moment a request *arrives*, and whether work was delivered is knowable only
/// from what goes back.
#[derive(Debug)]
pub struct RequestCounter {
    name: String,
    pricing: RequestPricing,
}

impl RequestCounter {
    /// A counter labelled `name`, pricing requests as `pricing` describes.
    #[must_use]
    pub fn new(name: impl Into<String>, pricing: RequestPricing) -> Self {
        Self {
            name: name.into(),
            pricing,
        }
    }
}

impl Meter for RequestCounter {
    fn name(&self) -> &str {
        &self.name
    }

    fn bill(&mut self, scenario: &Scenario) -> u64 {
        let prices = price_book(scenario);
        scenario.exchanges.iter().fold(0_u64, |total, exchange| {
            let units = match self.pricing {
                RequestPricing::Flat => prices.default_units,
                // A request with no `Mcp-Name` - `tasks/get`, a listing - has
                // no per-tool price to look up, so it falls to the default.
                // That is not a simplification, it is the actual limit: the
                // poll that finally delivers a task carries no name either.
                RequestPricing::Named => exchange
                    .name
                    .as_deref()
                    .and_then(|name| prices.names.get(name).copied())
                    .unwrap_or(prices.default_units),
            };
            total.saturating_add(units)
        })
    }
}

/// Charges for delivered work, using this project's engine.
///
/// Drives `mcp_usage_core` the way the Tower edge does: it remembers the call
/// that created each durable task so a later poll can be priced, and it
/// suppresses a repeated charge for a task whose terminal state is observed
/// more than once.
#[derive(Debug, Default)]
pub struct TerminalDelivery {
    name: String,
}

impl TerminalDelivery {
    /// A meter labelled `name` in the report.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

impl Meter for TerminalDelivery {
    fn name(&self) -> &str {
        &self.name
    }

    fn bill(&mut self, scenario: &Scenario) -> u64 {
        let prices = price_book(scenario);

        // State the real edge keeps: which call commissioned each task, and
        // which once-only charges have already been recorded.
        let mut task_origins: HashMap<String, Call> = HashMap::new();
        let mut settled: HashSet<String> = HashSet::new();
        let mut total = 0_u64;

        for exchange in &scenario.exchanges {
            let call = Call::new(Method::parse(&exchange.method), exchange.name.clone());
            let response = peek::response(&exchange.response);

            let task_id = response
                .task
                .as_ref()
                .and_then(|task| task.task_id.as_deref())
                .map(str::to_owned);

            // A `resultType: "task"` response is the server accepting work,
            // not performing it. Record what commissioned it so the poll that
            // finally delivers can be priced: `tasks/get` carries no name.
            if matches!(response.result_type, ResultType::Task)
                && let Some(id) = task_id.clone()
            {
                task_origins.entry(id).or_insert_with(|| call.clone());
            }

            let origin = task_id.as_ref().and_then(|id| task_origins.get(id));
            match decide_with_task_origin(&call, &response, &prices, origin) {
                Charge::Billable(billable) => {
                    // A terminal task keeps reporting `completed` to every
                    // later poll. The idempotency key on the task id is what
                    // stops the second look being a second invoice line.
                    let already_settled = billable
                        .idempotency_key
                        .is_some_and(|key| !settled.insert(key));
                    if !already_settled {
                        total = total.saturating_add(billable.units);
                    }
                }
                Charge::Free(_) => {}
            }
        }
        total
    }
}

fn price_book(scenario: &Scenario) -> PriceBook {
    scenario.named_units.iter().fold(
        PriceBook::flat(scenario.flat_units),
        |prices, (name, units)| prices.with_name(name.clone(), *units),
    )
}
