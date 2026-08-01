//! The billing decision: what a request/response pair is worth.
//!
//! # The rule
//!
//! **Charge on terminal delivery, never on request.**
//!
//! Every consequence a meter needs falls out of that one sentence, with no chain
//! correlation, no session, and no state:
//!
//! | Shape | Why it lands right |
//! |---|---|
//! | An MRTR chain of any length | Only its final round trip is `resultType: "complete"`, so it bills once |
//! | An abandoned chain | Never reaches `complete`, so it bills nothing |
//! | A task polled 300 times | Only the poll reporting `status: "completed"` delivered anything |
//! | A task that fails or is cancelled | Terminal, but delivered nothing, so nothing is owed |
//! | Discovery traffic | Never billable; it is the cost of connecting |
//! | A protocol error | The server failed; charging for that is indefensible |
//!
//! This replaced an earlier design that tried to collapse MRTR chains by
//! correlating their round trips. That design cannot be built by an intermediary:
//! `requestState` is "an opaque string meaningful only to the server", which
//! servers MUST integrity-protect with HMAC or AEAD, so the only correlator on
//! the wire is encrypted under a key the meter does not hold. Charging on
//! delivery needs no correlator, which is why it is both correct and cheaper.
//!
//! # Continuation state
//!
//! It does not consider whether the *request* was a continuation. Under the rule
//! above that fact is irrelevant to billing, and requiring it would force a body
//! peek on every `tools/call`. [`crate::peek::request`] still exposes it for the
//! cache, because MRTR-derived results MUST NOT be cached.
//!
//! # Replay
//!
//! Two double-billing vectors exist, and they are treated differently on purpose.
//!
//! A **terminal task can be polled forever.** Terminal state "does not change", so
//! every later `tasks/get` reports `completed` again. That is one unit of work
//! reported many times, so [`Billable::idempotency_key`] carries the `taskId` and
//! the caller MUST suppress repeats.
//!
//! A **replayed MRTR continuation** may produce a second `complete`. The spec
//! bounds the replay window but explicitly does not guarantee single use. We bill
//! it because the origin executed the tool a second time. Two completed executions
//! produce two charges.

use crate::peek::{ResponsePeek, ResultType};
use crate::price::PriceBook;
use crate::{Method, TaskStatus};

/// The call being priced, as known from its (trusted) headers.
///
/// `name` must already be decoded from the `Mcp-Name` sentinel form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    /// MCP method whose delivered work is being priced.
    pub method: Method,
    /// Decoded `Mcp-Name`, when the method carries one.
    pub name: Option<String>,
}

impl Call {
    /// Construct a classified call from trusted, decoded headers.
    #[must_use]
    pub fn new(method: Method, name: Option<String>) -> Self {
        Self { method, name }
    }
}

/// Non-identifying method category retained for a durable task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskOriginKind {
    /// A task created by `tools/call`.
    ToolsCall,
    /// A task created by `resources/read`.
    ResourcesRead,
    /// A task created by `prompts/get`.
    PromptsGet,
    /// A task created by an extension or unexpected method.
    Other,
}

impl TaskOriginKind {
    /// Reduce a method to a fixed category that cannot retain extension text.
    #[must_use]
    pub const fn from_method(method: &Method) -> Self {
        match method {
            Method::ToolsCall => Self::ToolsCall,
            Method::ResourcesRead => Self::ResourcesRead,
            Method::PromptsGet => Self::PromptsGet,
            _ => Self::Other,
        }
    }

    fn method(self) -> Method {
        match self {
            Self::ToolsCall => Method::ToolsCall,
            Self::ResourcesRead => Method::ResourcesRead,
            Self::PromptsGet => Method::PromptsGet,
            Self::Other => Method::Other("[redacted-task-origin]".to_owned()),
        }
    }
}

/// Pre-priced durable-task attribution without the original name or URI.
///
/// Construct this when a task is created, then persist it under the task ID.
/// The resolved units preserve named pricing while avoiding storage of a tool
/// name, prompt name, resource URI, or extension method string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskAttribution {
    origin_kind: TaskOriginKind,
    units: u64,
}

impl TaskAttribution {
    /// Construct an attribution from an already resolved method category and price.
    #[must_use]
    pub const fn new(origin_kind: TaskOriginKind, units: u64) -> Self {
        Self { origin_kind, units }
    }

    /// Resolve a call against the current price book and discard identifying text.
    #[must_use]
    pub fn from_call(call: &Call, prices: &PriceBook) -> Self {
        Self {
            origin_kind: TaskOriginKind::from_method(&call.method),
            units: prices.units_for(&call.method, call.name.as_deref()),
        }
    }

    /// Fixed, non-identifying category of the call that created the task.
    #[must_use]
    pub const fn origin_kind(self) -> TaskOriginKind {
        self.origin_kind
    }

    /// Units resolved when the task was created.
    #[must_use]
    pub const fn units(self) -> u64 {
        self.units
    }
}

/// A charge to record against a tenant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Billable {
    /// Method that originally commissioned the delivered work.
    pub method: Method,
    /// Decoded tool, prompt, or resource name when available.
    pub name: Option<String>,
    /// Integer units to record with the billing backend.
    pub units: u64,
    /// When present, the caller MUST NOT record this charge more than once for
    /// this key. Carried by task completions, whose terminal state can be
    /// re-observed by any number of later polls.
    pub idempotency_key: Option<String>,
}

/// Why a call was not billed. Kept specific so an operator answering "why is my
/// bill lower than my request count" gets a real answer from the metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FreeReason {
    /// `tools/list` and friends: the cost of connecting, not of work.
    Discovery,
    /// An interim `input_required` result. Nothing delivered yet.
    InterimInputRequired,
    /// A `CreateTaskResult`: work accepted, not performed.
    TaskCreated,
    /// A poll of a task still `working` or awaiting input.
    TaskInProgress,
    /// A task that reached a terminal state without delivering: failed, cancelled.
    TaskNotDelivered,
    /// `tasks/update` or `tasks/cancel`: driving a task, not receiving its output.
    TaskDrive,
    /// A completed task could not be joined to the call that created it.
    MissingTaskAttribution,
    /// A completed task omitted its required durable identifier, so repeat
    /// polls could not be made idempotent.
    MissingTaskId,
    /// A `subscriptions/listen` stream. Not a discrete call; see the findings doc.
    Subscription,
    /// A JSON-RPC error. The server did not deliver.
    ProtocolError,
    /// A result shape this build does not recognize. Fails toward not charging.
    UnrecognizedResult,
}

/// The verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Charge {
    /// Delivered work that should be recorded.
    Billable(Billable),
    /// An exchange for which no usage is owed.
    Free(FreeReason),
}

impl Charge {
    /// Units owed, or zero when free.
    #[must_use]
    pub fn units(&self) -> u64 {
        match self {
            Self::Billable(b) => b.units,
            Self::Free(_) => 0,
        }
    }

    /// Whether the verdict records delivered work (including zero-priced work).
    #[must_use]
    pub const fn is_billable(&self) -> bool {
        matches!(self, Self::Billable(_))
    }

    /// The idempotency key, when this charge carries one.
    #[must_use]
    pub fn idempotency_key(&self) -> Option<&str> {
        match self {
            Self::Billable(b) => b.idempotency_key.as_deref(),
            Self::Free(_) => None,
        }
    }
}

/// Decide what a completed request/response exchange is worth.
///
/// Ordering matters: errors are checked before anything else so a failure is
/// never billed regardless of which method produced it, and task handling
/// precedes the generic `resultType` arm because a task poll legitimately carries
/// `resultType: "complete"` while delivering nothing but a progress report.
#[must_use]
pub fn decide(call: &Call, response: &ResponsePeek, prices: &PriceBook) -> Charge {
    decide_with_task_origin(call, response, prices, None)
}

enum TaskPricing<'a> {
    Call(&'a Call),
    Attribution(&'a TaskAttribution),
}

/// Decide an exchange while supplying the call that created a polled task.
///
/// A `tasks/get` request carries no `Mcp-Name`, so its terminal response cannot
/// be priced correctly without the attribution captured from the original
/// `tools/call`. Callers must persist that association when they observe a
/// `resultType: "task"` response and pass it here on subsequent polls.
#[must_use]
pub fn decide_with_task_origin(
    call: &Call,
    response: &ResponsePeek,
    prices: &PriceBook,
    task_origin: Option<&Call>,
) -> Charge {
    decide_with_task_pricing(call, response, prices, task_origin.map(TaskPricing::Call))
}

/// Decide an exchange using pre-priced, non-identifying task attribution.
///
/// This is the privacy-preserving form for durable storage. The task's units
/// are resolved when it is created, so a later terminal poll does not need the
/// original name, URI, or extension method string.
#[must_use]
pub fn decide_with_task_attribution(
    call: &Call,
    response: &ResponsePeek,
    prices: &PriceBook,
    task_attribution: Option<&TaskAttribution>,
) -> Charge {
    decide_with_task_pricing(
        call,
        response,
        prices,
        task_attribution.map(TaskPricing::Attribution),
    )
}

fn decide_with_task_pricing(
    call: &Call,
    response: &ResponsePeek,
    prices: &PriceBook,
    task_pricing: Option<TaskPricing<'_>>,
) -> Charge {
    if response.is_error {
        return Charge::Free(FreeReason::ProtocolError);
    }

    if call.method.is_discovery() {
        return Charge::Free(FreeReason::Discovery);
    }

    if matches!(call.method, Method::SubscriptionsListen) {
        return Charge::Free(FreeReason::Subscription);
    }

    // A task handle accepts work but does not deliver it, regardless of the
    // seed status fields carried beside the discriminant.
    if matches!(response.result_type, ResultType::Task) {
        return Charge::Free(FreeReason::TaskCreated);
    }

    // Task lifecycle. A poll's own resultType says the poll succeeded, which says
    // nothing about the job, so the task status decides. The original call, not
    // `tasks/get`, owns the price and attribution.
    if matches!(call.method, Method::TasksGet)
        && let Some(task) = &response.task
    {
        return match &task.status {
            s if s.delivered() => {
                let Some(task_id) = task.task_id.clone() else {
                    return Charge::Free(FreeReason::MissingTaskId);
                };
                let Some(origin) = task_pricing else {
                    return Charge::Free(FreeReason::MissingTaskAttribution);
                };
                let (method, name, units) = match origin {
                    TaskPricing::Call(origin) => (
                        origin.method.clone(),
                        origin.name.clone(),
                        prices.units_for(&origin.method, origin.name.as_deref()),
                    ),
                    TaskPricing::Attribution(attribution) => {
                        (attribution.origin_kind.method(), None, attribution.units)
                    }
                };
                Charge::Billable(Billable {
                    method,
                    name,
                    units,
                    // A terminal task reports `completed` to every later poll.
                    idempotency_key: Some(task_id),
                })
            }
            s if s.is_terminal() => Charge::Free(FreeReason::TaskNotDelivered),
            TaskStatus::Working | TaskStatus::InputRequired => {
                Charge::Free(FreeReason::TaskInProgress)
            }
            _ => Charge::Free(FreeReason::TaskInProgress),
        };
    }

    if call.method.is_task_drive() {
        return Charge::Free(FreeReason::TaskDrive);
    }

    match &response.result_type {
        ResultType::Complete => Charge::Billable(Billable {
            method: call.method.clone(),
            name: call.name.clone(),
            units: prices.units_for(&call.method, call.name.as_deref()),
            idempotency_key: None,
        }),
        ResultType::InputRequired => Charge::Free(FreeReason::InterimInputRequired),
        ResultType::Task => Charge::Free(FreeReason::TaskCreated),
        ResultType::Unknown(_) | ResultType::Absent => Charge::Free(FreeReason::UnrecognizedResult),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peek::{TaskPeek, response as peek_response};
    use serde_json::json;

    fn call(method: Method, name: &str) -> Call {
        Call::new(method, Some(name.to_owned()))
    }

    fn book() -> PriceBook {
        PriceBook::flat(1)
    }

    fn complete() -> ResponsePeek {
        peek_response(&json!({ "result": { "resultType": "complete", "content": [] } }))
    }

    fn input_required() -> ResponsePeek {
        peek_response(&json!({
            "result": {
                "resultType": "input_required",
                "inputRequests": { "k": { "method": "elicitation/create" } },
                "requestState": "AEAD-protected blob"
            }
        }))
    }

    fn task_poll(status: &str, id: &str) -> ResponsePeek {
        peek_response(&json!({
            "result": { "resultType": "complete", "taskId": id, "status": status }
        }))
    }

    #[test]
    fn a_delivered_tool_call_bills_once() {
        let charge = decide(
            &call(Method::ToolsCall, "get_weather"),
            &complete(),
            &book(),
        );
        assert!(charge.is_billable());
        assert_eq!(charge.units(), 1);
        assert_eq!(charge.idempotency_key(), None);
    }

    #[test]
    fn an_mrtr_chain_of_any_length_bills_exactly_once() {
        // The property that motivated the whole crate. Rounds 1..n-1 are interim;
        // only the last one delivers.
        for rounds in 1..=10 {
            let c = call(Method::ToolsCall, "get_weather");
            let mut total = 0;
            for round in 1..=rounds {
                let response = if round == rounds {
                    complete()
                } else {
                    input_required()
                };
                total += decide(&c, &response, &book()).units();
            }
            assert_eq!(total, 1, "a {rounds}-round chain billed {total} units");
        }
    }

    #[test]
    fn an_abandoned_chain_bills_nothing() {
        let c = call(Method::ToolsCall, "get_weather");
        let total: u64 = (0..5)
            .map(|_| decide(&c, &input_required(), &book()).units())
            .sum();
        assert_eq!(total, 0);
    }

    #[test]
    fn mrtr_applies_to_resources_read_and_prompts_get_too() {
        for method in [Method::ResourcesRead, Method::PromptsGet] {
            let c = call(method.clone(), "x");
            assert_eq!(decide(&c, &input_required(), &book()).units(), 0);
            assert_eq!(decide(&c, &complete(), &book()).units(), 1);
        }
    }

    #[test]
    fn a_task_polled_many_times_bills_once() {
        let poll = Call::new(Method::TasksGet, None);
        let origin = call(Method::ToolsCall, "long_job");
        let mut total = 0;
        let mut billed_keys: Vec<String> = Vec::new();

        // 300 progress polls, then the completion.
        for _ in 0..300 {
            total += decide_with_task_origin(
                &poll,
                &task_poll("working", "tsk_1"),
                &book(),
                Some(&origin),
            )
            .units();
        }
        let final_charge = decide_with_task_origin(
            &poll,
            &task_poll("completed", "tsk_1"),
            &book(),
            Some(&origin),
        );
        if let Some(key) = final_charge.idempotency_key() {
            billed_keys.push(key.to_owned());
        }
        total += final_charge.units();

        assert_eq!(total, 1, "300 polls plus a completion billed {total} units");
        assert_eq!(billed_keys, vec!["tsk_1".to_owned()]);
    }

    #[test]
    fn a_completed_task_carries_its_id_so_repeat_polls_can_be_suppressed() {
        // Terminal state does not change, so nothing stops a client polling a
        // finished task forever. Core flags it; the caller enforces once-only.
        let poll = Call::new(Method::TasksGet, None);
        let origin = call(Method::ToolsCall, "long_job");
        let first = decide_with_task_origin(
            &poll,
            &task_poll("completed", "tsk_9"),
            &book(),
            Some(&origin),
        );
        let second = decide_with_task_origin(
            &poll,
            &task_poll("completed", "tsk_9"),
            &book(),
            Some(&origin),
        );

        assert!(first.is_billable() && second.is_billable());
        assert_eq!(first.idempotency_key(), Some("tsk_9"));
        assert_eq!(first.idempotency_key(), second.idempotency_key());
    }

    #[test]
    fn failed_and_cancelled_tasks_are_free() {
        let c = Call::new(Method::TasksGet, None);
        assert_eq!(
            decide(&c, &task_poll("failed", "t"), &book()),
            Charge::Free(FreeReason::TaskNotDelivered)
        );
        assert_eq!(
            decide(&c, &task_poll("cancelled", "t"), &book()),
            Charge::Free(FreeReason::TaskNotDelivered)
        );
    }

    #[test]
    fn a_task_awaiting_input_is_free() {
        let c = Call::new(Method::TasksGet, None);
        assert_eq!(
            decide(&c, &task_poll("input_required", "t"), &book()),
            Charge::Free(FreeReason::TaskInProgress)
        );
    }

    #[test]
    fn creating_a_task_is_not_delivering_it() {
        let created = peek_response(&json!({
            "result": { "resultType": "task", "taskId": "tsk_1", "status": "working" }
        }));
        assert_eq!(
            decide(&call(Method::ToolsCall, "t"), &created, &book()),
            Charge::Free(FreeReason::TaskCreated)
        );
    }

    #[test]
    fn task_drive_calls_are_free() {
        let ack = peek_response(&json!({ "result": { "resultType": "complete" } }));
        for method in [Method::TasksUpdate, Method::TasksCancel] {
            assert_eq!(
                decide(&Call::new(method, None), &ack, &book()),
                Charge::Free(FreeReason::TaskDrive)
            );
        }
    }

    #[test]
    fn discovery_is_never_billed() {
        for method in [
            Method::ToolsList,
            Method::PromptsList,
            Method::ResourcesList,
            Method::ResourcesTemplatesList,
            Method::ServerDiscover,
        ] {
            assert_eq!(
                decide(&Call::new(method, None), &complete(), &book()),
                Charge::Free(FreeReason::Discovery)
            );
        }
    }

    #[test]
    fn subscription_streams_are_free() {
        assert_eq!(
            decide(
                &Call::new(Method::SubscriptionsListen, None),
                &complete(),
                &book()
            ),
            Charge::Free(FreeReason::Subscription)
        );
    }

    #[test]
    fn errors_are_never_billed_whatever_the_method() {
        let err = peek_response(&json!({ "error": { "code": -32020, "message": "mismatch" } }));
        for method in [Method::ToolsCall, Method::TasksGet, Method::ResourcesRead] {
            assert_eq!(
                decide(&call(method, "x"), &err, &book()),
                Charge::Free(FreeReason::ProtocolError)
            );
        }
    }

    #[test]
    fn unrecognized_results_fail_toward_not_charging() {
        let weird = peek_response(&json!({ "result": { "resultType": "some_future_shape" } }));
        assert_eq!(
            decide(&call(Method::ToolsCall, "t"), &weird, &book()),
            Charge::Free(FreeReason::UnrecognizedResult)
        );

        let empty = peek_response(&json!({ "jsonrpc": "2.0" }));
        assert_eq!(
            decide(&call(Method::ToolsCall, "t"), &empty, &book()),
            Charge::Free(FreeReason::UnrecognizedResult)
        );
    }

    #[test]
    fn price_book_drives_the_units() {
        let prices = PriceBook::flat(1).with_name("expensive", 250);
        assert_eq!(
            decide(&call(Method::ToolsCall, "expensive"), &complete(), &prices).units(),
            250
        );
        assert_eq!(
            decide(&call(Method::ToolsCall, "cheap"), &complete(), &prices).units(),
            1
        );
    }

    #[test]
    fn a_zero_priced_tool_still_reports_as_billable_with_zero_units() {
        // Distinguishing "free by price" from "free by protocol shape" keeps the
        // metrics honest about why a call cost nothing.
        let prices = PriceBook::flat(5).with_name("loss_leader", 0);
        let charge = decide(
            &call(Method::ToolsCall, "loss_leader"),
            &complete(),
            &prices,
        );
        assert!(charge.is_billable());
        assert_eq!(charge.units(), 0);
    }

    #[test]
    fn an_unknown_task_status_is_not_billed() {
        let paused = peek_response(&json!({
            "result": { "resultType": "complete", "taskId": "t", "status": "paused" }
        }));
        assert!(!decide(&Call::new(Method::TasksGet, None), &paused, &book()).is_billable());
    }

    #[test]
    fn a_task_without_an_id_fails_toward_not_charging() {
        let peek = ResponsePeek {
            result_type: ResultType::Complete,
            is_error: false,
            task: Some(TaskPeek {
                task_id: None,
                status: TaskStatus::Completed,
            }),
        };
        let charge = decide_with_task_origin(
            &Call::new(Method::TasksGet, None),
            &peek,
            &book(),
            Some(&call(Method::ToolsCall, "j")),
        );
        assert_eq!(charge, Charge::Free(FreeReason::MissingTaskId));
    }

    #[test]
    fn a_completed_task_without_origin_fails_toward_not_charging() {
        let charge = decide(
            &Call::new(Method::TasksGet, None),
            &task_poll("completed", "tsk_orphan"),
            &book(),
        );
        assert_eq!(charge, Charge::Free(FreeReason::MissingTaskAttribution));
    }

    #[test]
    fn a_completed_task_uses_the_originating_tool_price() {
        let prices = PriceBook::flat(1).with_name("expensive_job", 250);
        let charge = decide_with_task_origin(
            &Call::new(Method::TasksGet, None),
            &task_poll("completed", "tsk_expensive"),
            &prices,
            Some(&call(Method::ToolsCall, "expensive_job")),
        );
        assert_eq!(charge.units(), 250);
        let Charge::Billable(billable) = charge else {
            panic!("completed task should be billable");
        };
        assert_eq!(billable.method, Method::ToolsCall);
        assert_eq!(billable.name.as_deref(), Some("expensive_job"));
    }

    #[test]
    fn prepriced_task_attribution_discards_identifying_names() {
        let prices = PriceBook::flat(1).with_name("file:///private/customer-record", 250);
        let origin = Call::new(
            Method::ResourcesRead,
            Some("file:///private/customer-record".to_owned()),
        );
        let attribution = TaskAttribution::from_call(&origin, &prices);

        assert_eq!(attribution.units(), 250);
        assert_eq!(attribution.origin_kind(), TaskOriginKind::ResourcesRead);
        assert!(!format!("{attribution:?}").contains("customer-record"));

        let charge = decide_with_task_attribution(
            &Call::new(Method::TasksGet, None),
            &task_poll("completed", "tsk_private"),
            &prices,
            Some(&attribution),
        );
        let Charge::Billable(billable) = charge else {
            panic!("completed task should be billable");
        };
        assert_eq!(billable.units, 250);
        assert_eq!(billable.method, Method::ResourcesRead);
        assert_eq!(billable.name, None);
    }
}
