# mcp-usage-store

Distributed durable-task attribution for UsageKit for MCP.

Enable `redis` for a reconnecting Redis or Valkey store, or `postgres` for a
pooled PostgreSQL store. Both backends hash tenant and task identifiers before
storage and preserve the first pre-priced attribution. Values contain only the
integer price and a fixed method category, never a tool name, prompt name,
resource URI, or extension method string. Backend operations default to a
two-second timeout. Use `RedisTaskStore::connect_with_timeout` or
`PostgresTaskStore::with_timeout` to set an application-specific bound.

[API documentation](https://docs.rs/mcp-usage-store) | [Project repository](https://github.com/pokitappz/mcp-usage-kit)
