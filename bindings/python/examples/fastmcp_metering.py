"""Meter a FastMCP server in process.

The sidecar meters an MCP server from outside, over HTTP. This does it from
inside, with no extra hop, using the same Rust engine and therefore the same
billing semantics.

Run: python examples/fastmcp_metering.py
"""

import asyncio

from fastmcp import Client, FastMCP
from fastmcp.server.middleware import Middleware

import mcp_usage_kit as usage


class Metering(Middleware):
    """Record what each delivered tool call is worth.

    FastMCP hands middleware a result object rather than a JSON-RPC envelope,
    so the envelope helpers do that translation in one tested place. Writing
    `{"result": {"resultType": ...}}` by hand puts a silent billing change one
    typo away.
    """

    def __init__(self, prices):
        self.meter = usage.Meter(prices)
        self.recorded = []

    async def on_call_tool(self, context, call_next):
        try:
            result = await call_next(context)
        except Exception:
            # A raised tool is a JSON-RPC error, and an error is never billed.
            # Deciding it explicitly keeps the reason in the ledger rather than
            # leaving a silent gap where a call used to be.
            self._record(context.message.name, usage.failed())
            raise
        self._record(context.message.name, usage.delivered())
        return result

    def _record(self, name, response):
        charge = self.meter.decide(
            method="tools/call", name=name, response=response
        )
        self.recorded.append((name, charge))
        if charge.billable:
            print(f"  billed {name}: {charge.units} units")
        else:
            print(f"  free   {name}: {charge.reason}")


async def main():
    prices = usage.PriceBook(
        default_units=1,
        names={"sum_numbers": 7, "cheap_probe": 0},
    )
    metering = Metering(prices)

    server = FastMCP("calc")
    server.add_middleware(metering)

    @server.tool
    def sum_numbers(a: int, b: int) -> int:
        """Add two integers."""
        return a + b

    @server.tool
    def cheap_probe() -> str:
        """A loss leader, priced at zero units."""
        return "ok"

    @server.tool
    def always_fails() -> str:
        """A tool that raises, to show an error is not billed."""
        raise RuntimeError("upstream exploded")

    async with Client(server) as client:
        print("tools/list (discovery is free, and never reaches on_call_tool)")
        await client.list_tools()

        print("calls:")
        await client.call_tool("sum_numbers", {"a": 2, "b": 3})
        await client.call_tool("cheap_probe", {})
        try:
            await client.call_tool("always_fails", {})
        except Exception:
            pass

    billed = sum(c.units for _, c in metering.recorded if c.billable)
    print()
    print(f"total billed: {billed} units across {len(metering.recorded)} calls")
    reasons = {name: c.reason for name, c in metering.recorded if not c.billable}
    print(f"not billed: {reasons}")


if __name__ == "__main__":
    asyncio.run(main())
