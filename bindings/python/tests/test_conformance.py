"""Run the shared conformance vectors against the Python binding.

These are the same `conformance/v1/cases.json` the Rust reference test runs.
The engine underneath is already the reference implementation, so what this
actually proves is the translation layer: that a Python dict becomes the same
JSON the Rust edge sees, that method and name arrive intact, that task origins
are wired through, and that free reasons surface under their stable wire names.

That shared vector file is the reason a second language is a week and not a
quarter.
"""

import json
import pathlib

import pytest

import mcp_usage_kit as usage

VECTORS = (
    pathlib.Path(__file__).resolve().parents[3]
    / "crates"
    / "mcp-usage-core"
    / "conformance"
    / "v1"
    / "cases.json"
)


def load_suite():
    suite = json.loads(VECTORS.read_text())
    assert suite["schemaVersion"] == usage.CONFORMANCE_SCHEMA_VERSION
    assert len(suite["cases"]) >= 10
    return suite


SUITE = load_suite()


def meter_for(case):
    return usage.Meter(
        usage.PriceBook(
            default_units=case["flatUnits"],
            names=case.get("namedUnits"),
        )
    )


def origin_for(case):
    origin = case.get("taskOrigin")
    return None if origin is None else (origin["method"], origin.get("name"))


@pytest.mark.parametrize("case", SUITE["cases"], ids=lambda c: c["id"])
def test_vector(case):
    charge = meter_for(case).decide(
        method=case["method"],
        name=case.get("name"),
        response=case["response"],
        task_origin=origin_for(case),
    )
    expected = case["expected"]

    if expected["kind"] == "billable":
        assert charge.billable, f"{case['id']}: expected billable, got {charge.reason}"
        assert charge.units == expected["units"], case["id"]
        assert charge.idempotency_key == expected.get("idempotencyKey"), case["id"]
    else:
        assert not charge.billable, f"{case['id']}: expected free, got {charge.units} units"
        assert charge.reason == expected["reason"], case["id"]
        assert charge.units == 0, case["id"]


def test_a_json_string_body_gives_the_same_verdict_as_a_mapping():
    # Callers reach the response either way; refusing one would only make them
    # round-trip through `json` to satisfy a type check.
    for case in SUITE["cases"]:
        meter = meter_for(case)
        as_mapping = meter.decide(
            method=case["method"],
            name=case.get("name"),
            response=case["response"],
            task_origin=origin_for(case),
        )
        as_text = meter.decide(
            method=case["method"],
            name=case.get("name"),
            response=json.dumps(case["response"]),
            task_origin=origin_for(case),
        )
        assert as_mapping.billable == as_text.billable, case["id"]
        assert as_mapping.units == as_text.units, case["id"]
        assert as_mapping.reason == as_text.reason, case["id"]
