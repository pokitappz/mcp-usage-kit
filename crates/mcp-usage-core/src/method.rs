//! The MCP method taxonomy, and the classifications the meter needs from it.
//!
//! A method arrives at an intermediary in the `Mcp-Method` header, which
//! 2026-07-28 makes REQUIRED on every request. Parsing it is therefore the whole
//! of request classification: no body, no allocation, one string comparison
//! against a small fixed set.
//!
//! Each classification maps to a billing or caching rule from the specification.
//! See `docs/spec-2026-07-28-findings.md`.

use std::fmt;

/// A method named in the `Mcp-Method` header.
///
/// `Other` carries anything outside the core protocol so that an unrecognized
/// method is classified conservatively rather than rejected: extensions define
/// their own methods, and a meter that 400s on every extension it has not been
/// taught about is worse than useless in front of a real server.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Method {
    /// Invoke a server tool.
    ToolsCall,
    /// List the tools exposed by the server.
    ToolsList,
    /// Read a resource by URI.
    ResourcesRead,
    /// List resources exposed by the server.
    ResourcesList,
    /// List URI templates exposed by the server.
    ResourcesTemplatesList,
    /// Render a named prompt.
    PromptsGet,
    /// List prompts exposed by the server.
    PromptsList,
    /// Discover server capabilities without an initialization handshake.
    ServerDiscover,
    /// Poll a durable task.
    TasksGet,
    /// Supply input requested by a durable task.
    TasksUpdate,
    /// Request cancellation of a durable task.
    TasksCancel,
    /// Open the long-lived change-notification stream.
    SubscriptionsListen,
    /// An extension method not known to this build.
    Other(String),
}

impl Method {
    /// Parse a `Mcp-Method` header value.
    ///
    /// Header *values* are case-sensitive (only field names are not), so this
    /// does not fold case: `Tools/Call` is not `tools/call`, and
    /// treating it as such would let a client dodge a per-method price rule by
    /// changing capitalization.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw {
            "tools/call" => Self::ToolsCall,
            "tools/list" => Self::ToolsList,
            "resources/read" => Self::ResourcesRead,
            "resources/list" => Self::ResourcesList,
            "resources/templates/list" => Self::ResourcesTemplatesList,
            "prompts/get" => Self::PromptsGet,
            "prompts/list" => Self::PromptsList,
            "server/discover" => Self::ServerDiscover,
            "tasks/get" => Self::TasksGet,
            "tasks/update" => Self::TasksUpdate,
            "tasks/cancel" => Self::TasksCancel,
            "subscriptions/listen" => Self::SubscriptionsListen,
            other => Self::Other(other.to_owned()),
        }
    }

    /// The canonical wire string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::ToolsCall => "tools/call",
            Self::ToolsList => "tools/list",
            Self::ResourcesRead => "resources/read",
            Self::ResourcesList => "resources/list",
            Self::ResourcesTemplatesList => "resources/templates/list",
            Self::PromptsGet => "prompts/get",
            Self::PromptsList => "prompts/list",
            Self::ServerDiscover => "server/discover",
            Self::TasksGet => "tasks/get",
            Self::TasksUpdate => "tasks/update",
            Self::TasksCancel => "tasks/cancel",
            Self::SubscriptionsListen => "subscriptions/listen",
            Self::Other(s) => s,
        }
    }

    /// Whether this method carries `Mcp-Name` (from `params.name` or `params.uri`).
    ///
    /// The spec requires the header only for `tools/call`, `resources/read`, and
    /// `prompts/get`. A meter pricing per tool can therefore only expect a name on
    /// these three, and must not treat its absence elsewhere as malformed.
    #[must_use]
    pub const fn carries_name(&self) -> bool {
        matches!(
            self,
            Self::ToolsCall | Self::ResourcesRead | Self::PromptsGet
        )
    }

    /// Whether the server is permitted to answer with an `InputRequiredResult`.
    ///
    /// The spec is exhaustive and closed here: servers MAY return one on
    /// `prompts/get`, `resources/read`, and `tools/call`, and "MUST NOT send
    /// `InputRequiredResult` responses on any other client requests."
    ///
    /// This is what bounds the body peek. Only these three can be a continuation,
    /// so only these three ever need their body inspected, and the other nine
    /// method values are decided from the header alone.
    #[must_use]
    pub const fn supports_mrtr(&self) -> bool {
        matches!(
            self,
            Self::ToolsCall | Self::ResourcesRead | Self::PromptsGet
        )
    }

    /// Whether results for this method carry caching hints (`ttlMs`, `cacheScope`).
    ///
    /// Note that `resources/read` appears in both this set and [`Self::supports_mrtr`].
    /// That overlap is exactly where the "MRTR results MUST NOT be cached" rule
    /// bites, and it is the reason the cache and the meter share one body peek.
    #[must_use]
    pub const fn is_cacheable(&self) -> bool {
        matches!(
            self,
            Self::ServerDiscover
                | Self::ToolsList
                | Self::PromptsList
                | Self::ResourcesList
                | Self::ResourcesTemplatesList
                | Self::ResourcesRead
        )
    }

    /// Discovery traffic: the client finding out what the server offers.
    ///
    /// Never billable. It is overhead the protocol imposes on the client, it is
    /// cacheable so it stops reaching the origin, and charging for it
    /// would price the act of connecting.
    #[must_use]
    pub const fn is_discovery(&self) -> bool {
        matches!(
            self,
            Self::ServerDiscover
                | Self::ToolsList
                | Self::PromptsList
                | Self::ResourcesList
                | Self::ResourcesTemplatesList
        )
    }

    /// Task lifecycle driving: polling, supplying input, cancelling.
    ///
    /// Never billable on its own. The work was commissioned by the originating
    /// `tools/call`; these are the client asking whether it is done yet. A meter
    /// that counts requests charges a 10-minute job polled every 2s roughly 300
    /// times for one unit of delivered work.
    #[must_use]
    pub const fn is_task_drive(&self) -> bool {
        matches!(self, Self::TasksGet | Self::TasksUpdate | Self::TasksCancel)
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_core_method() {
        for wire in [
            "tools/call",
            "tools/list",
            "resources/read",
            "resources/list",
            "resources/templates/list",
            "prompts/get",
            "prompts/list",
            "server/discover",
            "tasks/get",
            "tasks/update",
            "tasks/cancel",
            "subscriptions/listen",
        ] {
            let m = Method::parse(wire);
            assert!(
                !matches!(m, Method::Other(_)),
                "{wire} fell through to Other"
            );
            assert_eq!(m.as_str(), wire, "round trip failed for {wire}");
        }
    }

    #[test]
    fn unknown_methods_are_retained_verbatim() {
        let m = Method::parse("io.example/frobnicate");
        assert_eq!(m, Method::Other("io.example/frobnicate".to_owned()));
        assert_eq!(m.as_str(), "io.example/frobnicate");
    }

    #[test]
    fn method_values_are_case_sensitive() {
        // Header field *names* are case-insensitive; values are not. Folding case
        // here would let "Tools/Call" slip past a per-method rule.
        assert!(matches!(Method::parse("Tools/Call"), Method::Other(_)));
        assert!(matches!(Method::parse("TOOLS/LIST"), Method::Other(_)));
    }

    #[test]
    fn mrtr_set_is_exactly_the_three_the_spec_allows() {
        let allowed: Vec<Method> = [
            "tools/call",
            "tools/list",
            "resources/read",
            "resources/list",
            "resources/templates/list",
            "prompts/get",
            "prompts/list",
            "server/discover",
            "tasks/get",
            "tasks/update",
            "tasks/cancel",
            "subscriptions/listen",
            "io.example/other",
        ]
        .into_iter()
        .map(Method::parse)
        .filter(Method::supports_mrtr)
        .collect();

        assert_eq!(
            allowed,
            vec![Method::ToolsCall, Method::ResourcesRead, Method::PromptsGet]
        );
    }

    #[test]
    fn resources_read_is_both_cacheable_and_mrtr_capable() {
        // The overlap that forces cache and meter to share one body peek.
        let m = Method::ResourcesRead;
        assert!(m.is_cacheable());
        assert!(m.supports_mrtr());
    }

    #[test]
    fn discovery_is_cacheable_but_lists_are_not_mrtr_capable() {
        for m in [
            Method::ToolsList,
            Method::PromptsList,
            Method::ResourcesList,
            Method::ResourcesTemplatesList,
            Method::ServerDiscover,
        ] {
            assert!(m.is_cacheable(), "{m} should be cacheable");
            assert!(m.is_discovery(), "{m} should be discovery");
            assert!(
                !m.supports_mrtr(),
                "{m} must not accept InputRequiredResult"
            );
        }
    }

    #[test]
    fn task_drive_covers_get_update_cancel_only() {
        assert!(Method::TasksGet.is_task_drive());
        assert!(Method::TasksUpdate.is_task_drive());
        assert!(Method::TasksCancel.is_task_drive());
        assert!(!Method::ToolsCall.is_task_drive());
        // tasks/list was removed in this revision; it must not resolve to a known method.
        assert!(matches!(Method::parse("tasks/list"), Method::Other(_)));
    }

    #[test]
    fn name_bearing_methods_match_the_header_requirement() {
        assert!(Method::ToolsCall.carries_name());
        assert!(Method::ResourcesRead.carries_name());
        assert!(Method::PromptsGet.carries_name());
        assert!(!Method::ToolsList.carries_name());
        assert!(!Method::TasksGet.carries_name());
    }
}
