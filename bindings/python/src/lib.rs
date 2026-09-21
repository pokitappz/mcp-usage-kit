//! Python bindings for the MCP usage accounting engine.
//!
//! `mcp-usage-core` is pure, synchronous and free of I/O, which is exactly what
//! makes a second language cheap: this crate is a translation layer and nothing
//! else. Every accounting decision is made by the same Rust engine the sidecar
//! and the Tower edge use, so a `FastMCP` server in Python bills identically to a
//! Rust one by construction rather than by careful reimplementation.
//!
//! The shared conformance vectors are run from Python against this module in
//! CI. They are what proves the translation, since the engine underneath is
//! already the reference.
//!
//! Unlike the rest of the workspace this crate cannot `forbid(unsafe_code)`:
//! `PyO3`'s macros generate unsafe code. No unsafe is written by hand here.

#![warn(missing_docs, clippy::pedantic)]

use mcp_usage_core::{
    Call, Charge, FreeReason, LimitDecision, LimitReason, Limits, Method, PriceBook as CorePrices,
    Usage, assess_limits as core_assess_limits, decide_with_task_origin, name as core_name, peek,
};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyString};

/// The conformance schema this build implements.
const CONFORMANCE_SCHEMA_VERSION: u64 = 1;

/// Render an optional string the way Python does.
///
/// `{:?}` on an `Option` prints Rust's `Some(..)`/`None`, which is noise in a
/// Python traceback and actively misleading about what the attribute holds.
fn py_opt(value: Option<&String>) -> String {
    value.map_or_else(|| "None".to_owned(), |value| format!("{value:?}"))
}

/// Stable wire name for a free reason.
///
/// These strings are the contract the conformance vectors assert against, so
/// they are written out rather than derived from the Rust identifiers: a rename
/// upstream must not silently change what Python callers branch on.
const fn reason_name(reason: FreeReason) -> &'static str {
    match reason {
        FreeReason::Discovery => "discovery",
        FreeReason::InterimInputRequired => "interim_input_required",
        FreeReason::TaskCreated => "task_created",
        FreeReason::TaskInProgress => "task_in_progress",
        FreeReason::TaskNotDelivered => "task_not_delivered",
        FreeReason::TaskDrive => "task_drive",
        FreeReason::MissingTaskAttribution => "missing_task_attribution",
        FreeReason::MissingTaskId => "missing_task_id",
        FreeReason::Subscription => "subscription",
        FreeReason::ProtocolError => "protocol_error",
        FreeReason::UnrecognizedResult => "unrecognized_result",
    }
}

const fn limit_reason_name(reason: LimitReason) -> &'static str {
    match reason {
        LimitReason::QuotaExceeded => "quota_exceeded",
        LimitReason::SpendCapExceeded => "spend_cap_exceeded",
        LimitReason::ArithmeticOverflow => "arithmetic_overflow",
    }
}

/// How many units a delivered call is worth.
///
/// Units are integers, not currency. Turning units into money is the billing
/// provider's job, and keeping money out of here keeps rounding, currency and
/// tax out of the attribution engine where they would only cause trouble.
#[pyclass(module = "mcp_usage_kit", frozen, from_py_object)]
#[derive(Clone)]
pub struct PriceBook {
    inner: CorePrices,
}

#[pymethods]
impl PriceBook {
    /// Build a price book.
    ///
    /// `names` prices a tool name, a prompt name or a resource URI; `methods`
    /// prices a whole method. Resolution is most specific first: the name, then
    /// the method, then `default_units`.
    #[new]
    #[pyo3(signature = (default_units = 1, names = None, methods = None))]
    fn new(
        default_units: u64,
        names: Option<&Bound<'_, PyDict>>,
        methods: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Self> {
        let mut inner = CorePrices::flat(default_units);
        if let Some(names) = names {
            for (key, value) in names {
                inner = inner.with_name(key.extract::<String>()?, value.extract::<u64>()?);
            }
        }
        if let Some(methods) = methods {
            for (key, value) in methods {
                let method = Method::parse(&key.extract::<String>()?);
                inner = inner.with_method(&method, value.extract::<u64>()?);
            }
        }
        Ok(Self { inner })
    }

    /// Units a delivered call of `method` named `name` is worth.
    #[pyo3(signature = (method, name = None))]
    fn units_for(&self, method: &str, name: Option<&str>) -> u64 {
        self.inner.units_for(&Method::parse(method), name)
    }

    /// Price everything the same.
    #[staticmethod]
    fn flat(units: u64) -> Self {
        Self {
            inner: CorePrices::flat(units),
        }
    }

    fn __repr__(&self) -> String {
        format!(
            "PriceBook(default_units={}, names={}, methods={})",
            self.inner.default_units,
            self.inner.names.len(),
            self.inner.methods.len()
        )
    }
}

/// What a single exchange is worth.
#[pyclass(module = "mcp_usage_kit", frozen, get_all, skip_from_py_object)]
#[derive(Clone)]
pub struct ChargeResult {
    /// Whether this records delivered work. Zero-priced work is still billable.
    pub billable: bool,
    /// Units owed. Zero when free.
    pub units: u64,
    /// Why nothing is owed, or `None` when billable.
    pub reason: Option<String>,
    /// Method that commissioned the work, when billable.
    pub method: Option<String>,
    /// Decoded tool, prompt or resource name, when known.
    pub name: Option<String>,
    /// When present, the caller MUST NOT record this charge twice for this key.
    /// Carried by task completions, whose terminal state any later poll re-observes.
    pub idempotency_key: Option<String>,
}

#[pymethods]
impl ChargeResult {
    fn __repr__(&self) -> String {
        if self.billable {
            format!(
                "Charge(billable=True, units={}, name={}, idempotency_key={})",
                self.units,
                py_opt(self.name.as_ref()),
                py_opt(self.idempotency_key.as_ref())
            )
        } else {
            format!(
                "Charge(billable=False, reason={})",
                py_opt(self.reason.as_ref())
            )
        }
    }

    /// Truthy when the charge records delivered work.
    fn __bool__(&self) -> bool {
        self.billable
    }
}

impl From<Charge> for ChargeResult {
    fn from(charge: Charge) -> Self {
        match charge {
            Charge::Billable(billable) => Self {
                billable: true,
                units: billable.units,
                reason: None,
                method: Some(billable.method.as_str().to_owned()),
                name: billable.name,
                idempotency_key: billable.idempotency_key,
            },
            Charge::Free(reason) => Self {
                billable: false,
                units: 0,
                reason: Some(reason_name(reason).to_owned()),
                method: None,
                name: None,
                idempotency_key: None,
            },
        }
    }
}

/// The outcome of a quota and spend-cap check.
#[pyclass(module = "mcp_usage_kit", frozen, get_all, skip_from_py_object)]
#[derive(Clone)]
pub struct LimitOutcome {
    /// Whether the reservation may proceed.
    pub allowed: bool,
    /// Why it may not, or `None` when allowed.
    pub reason: Option<String>,
    /// Committed units after the reservation, when allowed.
    pub units: u64,
    /// Committed spend after the reservation, when allowed.
    pub spend_micros: u64,
}

#[pymethods]
impl LimitOutcome {
    fn __repr__(&self) -> String {
        if self.allowed {
            format!(
                "LimitOutcome(allowed=True, units={}, spend_micros={})",
                self.units, self.spend_micros
            )
        } else {
            format!(
                "LimitOutcome(allowed=False, reason={})",
                py_opt(self.reason.as_ref())
            )
        }
    }

    fn __bool__(&self) -> bool {
        self.allowed
    }
}

/// Accept either a mapping or an already-serialized JSON string.
///
/// `FastMCP` hands back a structured result, but plenty of code paths have the
/// raw body in hand instead. Refusing one of them would only make callers
/// round-trip through `json` to satisfy a type check.
fn to_json(response: &Bound<'_, PyAny>) -> PyResult<serde_json::Value> {
    if let Ok(text) = response.cast::<PyString>() {
        return serde_json::from_str(&text.to_cow()?).map_err(|error| {
            PyValueError::new_err(format!("response is not valid JSON: {error}"))
        });
    }
    pythonize::depythonize(response).map_err(|error| {
        PyValueError::new_err(format!(
            "response could not be read as JSON-compatible data: {error}"
        ))
    })
}

/// Accounting for one MCP server, against one price book.
#[pyclass(module = "mcp_usage_kit", frozen)]
pub struct Meter {
    prices: CorePrices,
}

#[pymethods]
impl Meter {
    /// Meter against `prices`, or one unit for everything when omitted.
    #[new]
    #[pyo3(signature = (prices = None))]
    fn new(prices: Option<PriceBook>) -> Self {
        Self {
            prices: prices.map_or_else(|| CorePrices::flat(1), |prices| prices.inner),
        }
    }

    /// Decide what one exchange is worth.
    ///
    /// `method` is the JSON-RPC method, `name` the tool, prompt or resource the
    /// call names, and `response` the JSON-RPC response as a mapping or a JSON
    /// string.
    ///
    /// `task_origin` is the call that created a durable task, as
    /// `(method, name)`. A `tasks/get` carries no name of its own, so without
    /// it a completed task cannot be priced and is recorded free rather than
    /// guessed at.
    #[pyo3(signature = (method, response, name = None, task_origin = None))]
    fn decide(
        &self,
        method: &str,
        response: &Bound<'_, PyAny>,
        name: Option<String>,
        task_origin: Option<(String, Option<String>)>,
    ) -> PyResult<ChargeResult> {
        let call = Call::new(Method::parse(method), name);
        let origin = task_origin.map(|(method, name)| Call::new(Method::parse(&method), name));
        let body = to_json(response)?;
        let observed = peek::response(&body);
        Ok(decide_with_task_origin(&call, &observed, &self.prices, origin.as_ref()).into())
    }

    /// Units a delivered call would be worth, ignoring the response.
    ///
    /// For pricing a call before it runs, such as quoting a payment. What is
    /// actually billed still depends on what came back.
    #[pyo3(signature = (method, name = None))]
    fn price(&self, method: &str, name: Option<&str>) -> u64 {
        self.prices.units_for(&Method::parse(method), name)
    }

    fn __repr__(&self) -> String {
        format!("Meter(default_units={})", self.prices.default_units)
    }
}

/// Assess a proposed usage increment against a quota and a spend cap.
///
/// `unit_price_micros` is the price of one unit in millionths of a currency
/// unit. Exact boundaries are allowed: a limit rejects only when the new total
/// would be greater.
#[pyfunction]
#[pyo3(signature = (
    committed_units = 0,
    committed_spend_micros = 0,
    requested_units = 0,
    unit_price_micros = 0,
    max_units = None,
    max_spend_micros = None,
))]
fn assess_limits(
    committed_units: u64,
    committed_spend_micros: u64,
    requested_units: u64,
    unit_price_micros: u64,
    max_units: Option<u64>,
    max_spend_micros: Option<u64>,
) -> LimitOutcome {
    let current = Usage {
        units: committed_units,
        spend_micros: committed_spend_micros,
    };
    let limits = Limits {
        max_units,
        max_spend_micros,
    };
    match core_assess_limits(current, requested_units, unit_price_micros, limits) {
        LimitDecision::Allowed(usage) => LimitOutcome {
            allowed: true,
            reason: None,
            units: usage.units,
            spend_micros: usage.spend_micros,
        },
        LimitDecision::Rejected(reason) => LimitOutcome {
            allowed: false,
            reason: Some(limit_reason_name(reason).to_owned()),
            units: 0,
            spend_micros: 0,
        },
    }
}

/// Decode an `Mcp-Name` header value.
///
/// The header carries a Base64 sentinel form for names that are not plain
/// ASCII. Pricing against the raw header value silently charges every
/// non-ASCII-named tool the default.
#[pyfunction]
fn decode_name(header_value: &str) -> PyResult<String> {
    core_name::decode(header_value)
        .map(std::borrow::Cow::into_owned)
        .map_err(|error| PyValueError::new_err(format!("invalid Mcp-Name header: {error}")))
}

// ------------------------------------------------------------- envelopes

/// Build a JSON-RPC response envelope as a Python mapping.
fn envelope(py: Python<'_>, body: &serde_json::Value) -> PyResult<Py<PyAny>> {
    pythonize::pythonize(py, body)
        .map(pyo3::Bound::unbind)
        .map_err(|error| PyValueError::new_err(format!("could not build a response: {error}")))
}

/// A response for work that was delivered.
///
/// A framework hands its middleware a result object, not a JSON-RPC envelope,
/// so these exist to keep the translation in one tested place. Hand-writing
/// `{"result": {"resultType": ...}}` puts a silent billing change one typo
/// away.
#[pyfunction]
fn delivered(py: Python<'_>) -> PyResult<Py<PyAny>> {
    envelope(
        py,
        &serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {"resultType": "complete"}}),
    )
}

/// A response for an interim round trip that is waiting on the caller.
#[pyfunction]
fn awaiting_input(py: Python<'_>) -> PyResult<Py<PyAny>> {
    envelope(
        py,
        &serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {"resultType": "input_required"}}),
    )
}

/// A JSON-RPC error response. Never billed.
#[pyfunction]
#[pyo3(signature = (code = -32603, message = "error"))]
fn failed(py: Python<'_>, code: i64, message: &str) -> PyResult<Py<PyAny>> {
    envelope(
        py,
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "error": {"code": code, "message": message}
        }),
    )
}

/// A response accepting work as a durable task. Never billed on its own.
#[pyfunction]
#[pyo3(signature = (task_id, status = "working"))]
fn task_created(py: Python<'_>, task_id: &str, status: &str) -> PyResult<Py<PyAny>> {
    envelope(
        py,
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {"resultType": "task", "taskId": task_id, "status": status}
        }),
    )
}

/// A response to polling a durable task.
///
/// Billed once, on the poll that first observes `completed`, and only when the
/// originating call is supplied as `task_origin`.
#[pyfunction]
#[pyo3(signature = (task_id, status))]
fn task_poll(py: Python<'_>, task_id: &str, status: &str) -> PyResult<Py<PyAny>> {
    envelope(
        py,
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {"resultType": "complete", "taskId": task_id, "status": status}
        }),
    )
}

/// Protocol-correct usage accounting for MCP servers.
#[pymodule]
fn mcp_usage_kit(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PriceBook>()?;
    module.add_class::<Meter>()?;
    module.add_class::<ChargeResult>()?;
    module.add_class::<LimitOutcome>()?;
    module.add_function(wrap_pyfunction!(assess_limits, module)?)?;
    module.add_function(wrap_pyfunction!(decode_name, module)?)?;
    module.add_function(wrap_pyfunction!(delivered, module)?)?;
    module.add_function(wrap_pyfunction!(awaiting_input, module)?)?;
    module.add_function(wrap_pyfunction!(failed, module)?)?;
    module.add_function(wrap_pyfunction!(task_created, module)?)?;
    module.add_function(wrap_pyfunction!(task_poll, module)?)?;
    module.add("__version__", env!("CARGO_PKG_VERSION"))?;
    module.add("CONFORMANCE_SCHEMA_VERSION", CONFORMANCE_SCHEMA_VERSION)?;
    Ok(())
}
