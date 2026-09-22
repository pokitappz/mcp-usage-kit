"""The surface a FastMCP server actually touches."""

import pytest

import mcp_usage_kit as usage

COMPLETE = {"jsonrpc": "2.0", "id": 1, "result": {"resultType": "complete", "content": []}}
ERROR = {"jsonrpc": "2.0", "id": 1, "error": {"code": -32603, "message": "failed"}}


def test_pricing_resolves_name_then_method_then_default():
    prices = usage.PriceBook(
        default_units=1,
        names={"expensive": 100},
        methods={"resources/read": 5},
    )
    assert prices.units_for("tools/call", "expensive") == 100
    assert prices.units_for("resources/read", "other") == 5
    assert prices.units_for("tools/call", "other") == 1
    # A zero-priced name is the supported way to expose a loss leader without
    # carving it out of the billing path.
    assert usage.PriceBook(default_units=9, names={"free": 0}).units_for("tools/call", "free") == 0


def test_a_delivered_call_bills_its_configured_units():
    meter = usage.Meter(usage.PriceBook(default_units=1, names={"sum": 7}))
    charge = meter.decide(method="tools/call", name="sum", response=COMPLETE)
    assert charge.billable
    assert charge.units == 7
    assert charge.name == "sum"
    assert charge.method == "tools/call"
    assert charge.idempotency_key is None
    assert bool(charge) is True


def test_an_error_is_free_and_says_why():
    meter = usage.Meter(usage.PriceBook(default_units=7))
    charge = meter.decide(method="tools/call", name="sum", response=ERROR)
    assert not charge.billable
    assert charge.units == 0
    assert charge.reason == "protocol_error"
    assert bool(charge) is False


def test_discovery_is_free():
    charge = usage.Meter().decide(method="tools/list", response=COMPLETE)
    assert not charge.billable
    assert charge.reason == "discovery"


def test_a_completed_task_needs_its_origin_to_be_priced():
    meter = usage.Meter(usage.PriceBook(default_units=1, names={"report": 25}))
    poll = {
        "jsonrpc": "2.0",
        "id": 3,
        "result": {"resultType": "complete", "taskId": "task-1", "status": "completed", "result": {}},
    }

    # A tasks/get carries no name of its own, so without the origin a completed
    # task cannot be priced and is recorded free rather than guessed at.
    unattributed = meter.decide(method="tasks/get", response=poll)
    assert not unattributed.billable
    assert unattributed.reason == "missing_task_attribution"

    attributed = meter.decide(
        method="tasks/get", response=poll, task_origin=("tools/call", "report")
    )
    assert attributed.billable
    assert attributed.units == 25
    # The key a caller must deduplicate on: any later poll re-observes the same
    # terminal state.
    assert attributed.idempotency_key == "task-1"


def test_a_default_meter_prices_everything_at_one():
    assert usage.Meter().price("tools/call", "anything") == 1
    assert usage.Meter().decide(method="tools/call", name="x", response=COMPLETE).units == 1


def test_price_quotes_a_call_before_it_runs():
    meter = usage.Meter(usage.PriceBook(default_units=1, names={"sum": 7}))
    assert meter.price("tools/call", "sum") == 7
    assert meter.price("tools/call") == 1


def test_a_malformed_response_is_refused_rather_than_guessed_at():
    meter = usage.Meter()
    with pytest.raises(ValueError, match="not valid JSON"):
        meter.decide(method="tools/call", response="{not json")
    with pytest.raises(ValueError, match="JSON-compatible"):
        meter.decide(method="tools/call", response=object())


def test_limits_reject_only_when_the_new_total_would_be_greater():
    at_the_boundary = usage.assess_limits(
        committed_units=8, requested_units=2, max_units=10
    )
    assert at_the_boundary.allowed
    assert at_the_boundary.units == 10
    assert bool(at_the_boundary) is True

    over = usage.assess_limits(committed_units=8, requested_units=3, max_units=10)
    assert not over.allowed
    assert over.reason == "quota_exceeded"


def test_a_spend_cap_is_checked_after_the_quota():
    # 800 + 2 * 100 lands exactly on the cap, and exact boundaries are allowed.
    at_the_cap = usage.assess_limits(
        committed_spend_micros=800,
        requested_units=2,
        unit_price_micros=100,
        max_spend_micros=1000,
    )
    assert at_the_cap.allowed
    assert at_the_cap.spend_micros == 1000

    # One unit further and the total would be greater than the cap.
    over = usage.assess_limits(
        committed_spend_micros=900,
        requested_units=2,
        unit_price_micros=100,
        max_spend_micros=1000,
    )
    assert not over.allowed
    assert over.reason == "spend_cap_exceeded"

    # The quota is checked first, so a request that breaks both reports the
    # quota rather than the cap.
    both = usage.assess_limits(
        committed_units=10,
        committed_spend_micros=1000,
        requested_units=1,
        unit_price_micros=100,
        max_units=10,
        max_spend_micros=1000,
    )
    assert both.reason == "quota_exceeded"


def test_arithmetic_that_cannot_be_represented_is_rejected_not_wrapped():
    outcome = usage.assess_limits(
        committed_units=2**64 - 1, requested_units=2, max_units=None
    )
    assert not outcome.allowed
    assert outcome.reason == "usage_unrepresentable"


def test_unbounded_limits_admit_everything():
    outcome = usage.assess_limits(committed_units=10**9, requested_units=10**6)
    assert outcome.allowed


def test_limit_reasons_match_the_codes_the_sidecar_returns():
    # One variant, one wire name. A caller moving from the sidecar to the
    # in-process binding must not silently stop matching on these. The
    # sidecar's own list is in crates/mcp-usage-edge/src/admission.rs.
    assert usage.assess_limits(committed_units=2, max_units=1).reason == "quota_exceeded"
    assert (
        usage.assess_limits(
            committed_spend_micros=2, requested_units=0, max_spend_micros=1
        ).reason
        == "spend_cap_exceeded"
    )
    assert (
        usage.assess_limits(committed_units=2**64 - 1, requested_units=2).reason
        == "usage_unrepresentable"
    )


def test_decide_decodes_an_encoded_tool_name_before_pricing():
    # The obvious middleware reads Mcp-Name and passes it straight in. An
    # encoded value matches nothing in the price book, which would silently
    # charge every non-ASCII-named tool the default instead of its own price.
    meter = usage.Meter(usage.PriceBook(default_units=1, names={"h\u00e9llo": 7}))
    encoded = "=?base64?aMOpbGxv?="

    assert meter.price("tools/call", encoded) == 7
    charge = meter.decide(
        method="tools/call", name=encoded, response=usage.delivered()
    )
    assert charge.billable
    assert charge.units == 7, "an encoded name must be priced as the tool it names"
    assert charge.name == "h\u00e9llo", "and reported decoded"


def test_a_malformed_encoded_name_is_refused_rather_than_mispriced():
    meter = usage.Meter()
    with pytest.raises(ValueError, match="invalid tool name"):
        meter.decide(
            method="tools/call",
            name="=?base64?!!!not-base64!!!?=",
            response=usage.delivered(),
        )


def test_the_name_header_sentinel_is_decoded():
    # Pricing against the raw header silently charges every non-ASCII-named
    # tool the default, so the sentinel has to be decoded before lookup.
    assert usage.decode_name("plain_tool") == "plain_tool"

    # The spec's own example, which decodes to a value wearing the markers.
    assert usage.decode_name("=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?=") == "=?base64?literal?="

    # Standard alphabet, not URL-safe: the example above contains a "/".
    assert usage.decode_name("=?base64?aMOpbGxv?=") == "h\u00e9llo"

    # Only a value wearing both markers is treated as encoded.
    assert usage.decode_name("=?base64?incomplete") == "=?base64?incomplete"

    with pytest.raises(ValueError, match="Mcp-Name"):
        usage.decode_name("=?base64?!!!not-base64!!!?=")


def test_the_module_reports_its_version_and_schema():
    assert usage.__version__
    assert usage.CONFORMANCE_SCHEMA_VERSION == 1


def test_repr_is_useful_in_a_traceback():
    charge = usage.Meter(usage.PriceBook(names={"sum": 7})).decide(
        method="tools/call", name="sum", response=COMPLETE
    )
    assert "billable=True" in repr(charge)
    assert "units=7" in repr(charge)
    # Rust's Option must not leak into a Python traceback.
    assert "Some(" not in repr(charge)
    free = usage.Meter().decide(method="tools/list", response=COMPLETE)
    assert "Some(" not in repr(free)
    rejected = usage.assess_limits(committed_units=11, max_units=10)
    assert "Some(" not in repr(rejected)
    assert "quota_exceeded" in repr(rejected)
    # An allowed outcome carries no reason, so its repr does not show one.
    assert "reason" not in repr(usage.assess_limits(requested_units=1))
    assert "PriceBook(" in repr(usage.PriceBook())
    assert "Meter(" in repr(usage.Meter())
