# mcp-usage-kit

`mcp-usage-kit` is the public facade for protocol-correct MCP usage accounting. It wraps
an rmcp service with a Tower layer, bills terminal delivery once, keeps protocol
traffic free, and re-exports the billing, protocol, and edge APIs.

```sh
cargo add mcp-usage-kit
```

See the [workspace README](https://github.com/pokitappz/mcp-usage-kit) for the
architecture, billing rules, example server, and operational bounds.

[API documentation](https://docs.rs/mcp-usage-kit) | [Project site](https://pokitappz.github.io/mcp-usage-kit/)

Licensed under Apache-2.0.
