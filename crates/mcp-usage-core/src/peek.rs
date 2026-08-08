//! The bounded body inspections the spec forces on a meter, and nothing more.
//!
//! Two facts a meter needs are not mirrored into any header.
//!
//! **Is this request a continuation?** A retried MRTR round trip is an ordinary
//! `tools/call` with an ordinary `Mcp-Method: tools/call` header. What marks it is
//! `inputResponses` or `requestState` under `params`, both body fields. The spec
//! uses exactly this test itself when defining what must not be cached:
//! "requests carrying `inputResponses` or `requestState`". We do not attempt to
//! read `requestState`; it is "an opaque string meaningful only to the server",
//! integrity-protected with HMAC or AEAD, and its contents are none of our
//! business. Its mere presence is the signal.
//!
//! **Did this response deliver the work?** `resultType` distinguishes a finished
//! result from an interim one, and for the tasks extension the task `status`
//! distinguishes a finished job from a progress report. Both live in the response
//! body.
//!
//! The cost of these peeks is bounded by [`crate::Method::supports_mrtr`]: only
//! three of the twelve core methods can ever be a continuation, so nine of them
//! are decided from headers alone.

use serde_json::Value;
use std::fmt;

/// The `resultType` discriminant on an MCP result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResultType {
    /// A finished result. The work was delivered.
    Complete,
    /// An interim result asking for more input. Nothing was delivered.
    InputRequired,
    /// A `CreateTaskResult`: the work was accepted, not performed.
    Task,
    /// A discriminant this build does not know. Never billed, so an unknown
    /// future result shape fails to the customer's advantage rather than ours.
    Unknown(String),
    /// No `resultType` present on a result. Only legacy servers do this, and the
    /// version guard should already have rejected them.
    Absent,
}

/// Status of a task under the `io.modelcontextprotocol/tasks` extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskStatus {
    /// Work is still executing.
    Working,
    /// Work is paused until the client supplies input.
    InputRequired,
    /// Work finished and its result is available.
    Completed,
    /// Work terminated with a protocol-level failure.
    Failed,
    /// Work was cancelled before delivery.
    Cancelled,
    /// A status introduced after this build was published.
    Unknown(String),
}

impl TaskStatus {
    /// Parse a task status while preserving unknown future values.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw {
            "working" => Self::Working,
            "input_required" => Self::InputRequired,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            other => Self::Unknown(other.to_owned()),
        }
    }

    /// Whether the task has reached a state it will never leave.
    ///
    /// `completed`, `failed`, and `cancelled` are terminal per the extension.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    /// Whether the task delivered the work that was originally asked for.
    ///
    /// Only `completed` did. `failed` and `cancelled` are terminal but delivered
    /// nothing, and billing for them would charge for the server's own failures.
    #[must_use]
    pub const fn delivered(&self) -> bool {
        matches!(self, Self::Completed)
    }
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Working => "working",
            Self::InputRequired => "input_required",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Unknown(s) => s,
        })
    }
}

/// What a request body reveals that its headers cannot.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RequestPeek {
    /// The request carries `inputResponses` or `requestState`, so it resumes an
    /// earlier round trip rather than starting fresh.
    pub is_continuation: bool,
}

/// What a response body reveals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponsePeek {
    /// The `resultType` discriminant, or [`ResultType::Absent`].
    pub result_type: ResultType,
    /// The response is a JSON-RPC error rather than a result.
    pub is_error: bool,
    /// Task state, when the result carries one.
    pub task: Option<TaskPeek>,
}

impl ResponsePeek {
    /// Read an absent `resultType` as delivered work, for a legacy request.
    ///
    /// `resultType` arrived with the interim-result machinery in
    /// [`crate::version::HEADER_VALIDATION_SINCE`]. A server answering an older client
    /// has no way to express "this is interim" and no reason to: on those revisions a
    /// JSON-RPC `result` is the delivered work, full stop. Treating its absence as
    /// unrecognized - correct for a modern response, where a missing discriminant means
    /// something unparsed - would make every legacy call free.
    ///
    /// Apply this only when the request itself was classified as legacy. A modern
    /// response missing its discriminant must stay unrecognized: there, absence means
    /// the meter failed to understand the result, and guessing would bill for work that
    /// may never have been delivered.
    #[must_use]
    pub fn with_legacy_delivery(mut self) -> Self {
        if matches!(self.result_type, ResultType::Absent) && !self.is_error {
            self.result_type = ResultType::Complete;
        }
        self
    }

    /// A response carrying nothing the meter recognizes.
    #[must_use]
    pub const fn unrecognized() -> Self {
        Self {
            result_type: ResultType::Absent,
            is_error: false,
            task: None,
        }
    }
}

/// Task identity and state lifted out of a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskPeek {
    /// The durable handle. Doubles as the idempotency key for billing, because a
    /// terminal task can be polled any number of times and every one of those
    /// polls reports `completed`.
    pub task_id: Option<String>,
    /// Current task lifecycle status.
    pub status: TaskStatus,
}

/// Inspect a request body for continuation markers.
///
/// Accepts an already-parsed [`Value`] so the caller can decide how the body was
/// obtained (borrowed from a buffer in-process, or parsed once at the edge) and
/// so this stays free of I/O.
#[must_use]
pub fn request(body: &Value) -> RequestPeek {
    let Some(params) = body.get("params") else {
        return RequestPeek::default();
    };
    RequestPeek {
        is_continuation: params.get("inputResponses").is_some_and(|v| !v.is_null())
            || params.get("requestState").is_some_and(|v| !v.is_null()),
    }
}

/// Inspect a response body for delivery signals.
#[must_use]
pub fn response(body: &Value) -> ResponsePeek {
    if body.get("error").is_some_and(|v| !v.is_null()) {
        return ResponsePeek {
            result_type: ResultType::Absent,
            is_error: true,
            task: None,
        };
    }

    let Some(result) = body.get("result") else {
        return ResponsePeek::unrecognized();
    };

    let result_type = match result.get("resultType").and_then(Value::as_str) {
        Some("complete") => ResultType::Complete,
        Some("input_required") => ResultType::InputRequired,
        Some("task") => ResultType::Task,
        Some(other) => ResultType::Unknown(other.to_owned()),
        None => ResultType::Absent,
    };

    ResponsePeek {
        result_type,
        is_error: false,
        task: peek_task(result),
    }
}

/// Lift task identity and status out of a result.
///
/// Handles both shapes the extension produces: a `CreateTaskResult`, whose task
/// fields sit on the result itself, and a `tasks/get` result, which *is* the task.
/// Both put `status` and `taskId` at the top level of the result, so one lookup
/// serves both.
fn peek_task(result: &Value) -> Option<TaskPeek> {
    let status = result.get("status").and_then(Value::as_str)?;
    Some(TaskPeek {
        task_id: result
            .get("taskId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        status: TaskStatus::parse(status),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_fresh_tool_call_is_not_a_continuation() {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "get_weather", "arguments": { "location": "Seattle, WA" } }
        });
        assert!(!request(&body).is_continuation);
    }

    #[test]
    fn input_responses_mark_a_continuation() {
        // The retry shape from the Tools page: same tool, new id, plus responses.
        let body = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "get_weather",
                "arguments": { "location": "New York" },
                "inputResponses": {
                    "github_login": { "action": "accept", "content": { "name": "octocat" } }
                },
                "requestState": "eyJsb2NhdGlvbiI6Ik5ldyBZb3JrIn0..."
            }
        });
        assert!(request(&body).is_continuation);
    }

    #[test]
    fn request_state_alone_marks_a_continuation() {
        // requestState is optional and may arrive without inputRequests, in which
        // case the client "MAY retry the original request immediately".
        let body = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "t", "requestState": "AEAD-protected blob" }
        });
        assert!(request(&body).is_continuation);
    }

    #[test]
    fn we_never_look_inside_request_state() {
        // Clients MUST NOT parse it and neither do we: an opaque, possibly
        // encrypted blob is treated purely as a presence flag.
        for state in [
            json!("AEAD-protected blob"),
            json!(""),
            json!("////not base64"),
        ] {
            let body = json!({ "method": "tools/call", "params": { "requestState": state } });
            assert!(request(&body).is_continuation);
        }
    }

    #[test]
    fn null_markers_do_not_count_as_present() {
        let body = json!({
            "method": "tools/call",
            "params": { "name": "t", "inputResponses": null, "requestState": null }
        });
        assert!(!request(&body).is_continuation);
    }

    #[test]
    fn a_body_without_params_is_not_a_continuation() {
        assert!(!request(&json!({ "method": "tools/list" })).is_continuation);
        assert!(!request(&json!({})).is_continuation);
    }

    #[test]
    fn reads_the_three_result_types() {
        let complete = json!({ "id": 2, "result": { "resultType": "complete", "content": [] } });
        assert_eq!(response(&complete).result_type, ResultType::Complete);

        let interim = json!({
            "id": 1,
            "result": {
                "resultType": "input_required",
                "inputRequests": { "github_login": { "method": "elicitation/create" } },
                "requestState": "AEAD-protected blob"
            }
        });
        assert_eq!(response(&interim).result_type, ResultType::InputRequired);

        let task = json!({
            "id": 1,
            "result": { "resultType": "task", "taskId": "tsk_1", "status": "working", "ttlMs": 60000 }
        });
        assert_eq!(response(&task).result_type, ResultType::Task);
    }

    #[test]
    fn errors_are_distinguished_from_results() {
        let err = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": { "code": -32020, "message": "Header mismatch" }
        });
        let peek = response(&err);
        assert!(peek.is_error);
        assert_eq!(peek.result_type, ResultType::Absent);
    }

    #[test]
    fn unknown_result_types_are_preserved_not_coerced() {
        let body = json!({ "result": { "resultType": "some_future_shape" } });
        assert_eq!(
            response(&body).result_type,
            ResultType::Unknown("some_future_shape".to_owned())
        );
    }

    #[test]
    fn task_polls_carry_status_and_id() {
        let working = json!({
            "result": { "resultType": "complete", "taskId": "tsk_abc", "status": "working" }
        });
        let peek = response(&working);
        let task = peek.task.expect("task present");
        assert_eq!(task.status, TaskStatus::Working);
        assert_eq!(task.task_id.as_deref(), Some("tsk_abc"));
        assert!(!task.status.is_terminal());
        assert!(!task.status.delivered());
    }

    #[test]
    fn terminal_statuses_are_classified_correctly() {
        assert!(TaskStatus::Completed.is_terminal() && TaskStatus::Completed.delivered());
        // Terminal, but nothing was delivered, so nothing is owed.
        assert!(TaskStatus::Failed.is_terminal() && !TaskStatus::Failed.delivered());
        assert!(TaskStatus::Cancelled.is_terminal() && !TaskStatus::Cancelled.delivered());
        assert!(!TaskStatus::Working.is_terminal() && !TaskStatus::Working.delivered());
        assert!(!TaskStatus::InputRequired.is_terminal());
        let unknown = TaskStatus::Unknown("paused".to_owned());
        assert!(!unknown.is_terminal() && !unknown.delivered());
    }

    #[test]
    fn task_status_round_trips_through_display() {
        for raw in [
            "working",
            "input_required",
            "completed",
            "failed",
            "cancelled",
        ] {
            assert_eq!(TaskStatus::parse(raw).to_string(), raw);
        }
    }

    #[test]
    fn a_plain_result_carries_no_task() {
        let body = json!({ "result": { "resultType": "complete", "content": [] } });
        assert!(response(&body).task.is_none());
    }

    #[test]
    fn a_response_with_neither_result_nor_error_is_unrecognized() {
        assert_eq!(
            response(&json!({ "jsonrpc": "2.0" })),
            ResponsePeek::unrecognized()
        );
    }
}
