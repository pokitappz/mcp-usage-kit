import pytest

from mcp_usage_kit import Meter, delivered, failed


def test_json_conversion_preserves_documented_inputs():
    meter = Meter()
    envelope = delivered()
    envelope["unused"] = [None, True, False, -1, 2**64 - 1, 1.5, "é", (1, 2)]
    assert meter.decide("tools/call", envelope).billable
    assert failed(message="é")["error"]["message"] == "é"


@pytest.mark.parametrize("value", [2**64, -(2**63) - 1, float("inf"), float("nan"), object(), {1: "value"}])
def test_invalid_values_are_rejected_without_echoing_data(value):
    with pytest.raises(ValueError):
        Meter().decide("tools/call", {"private-marker": value})


def test_cycles_and_excessive_nesting_are_rejected():
    cycle = []
    cycle.append(cycle)
    with pytest.raises(ValueError):
        Meter().decide("tools/call", cycle)
    nested = None
    for _ in range(130):
        nested = [nested]
    with pytest.raises(ValueError):
        Meter().decide("tools/call", nested)


def test_shared_acyclic_values_are_accepted_and_node_budget_is_enforced():
    common = [1, 2]
    data = delivered()
    data["unused"] = [common, common]
    assert Meter().decide("tools/call", data).billable
    with pytest.raises(ValueError):
        Meter().decide("tools/call", [None] * 100_001)
