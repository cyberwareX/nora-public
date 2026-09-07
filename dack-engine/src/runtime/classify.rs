//! Map OpenClaude's raw permission event `(tool_name, input)` to a [`ToolClass`] +
//! optional target path (grounded against OpenClaude 0.15.0).
//!
//! The SDK/gRPC permission callback hands us `(name, input, toolUseID)` — NOT a
//! pre-classified event — so the responder derives the class here. **Fail-closed:**
//! anything unrecognized → [`ToolClass::Other`], which no state scopes by default, so an
//! unknown or future tool is denied until explicitly allowed.
//!
//! Capability identity:
//! - **Reversible capabilities are MCP tools** (`mcp__twitter__post`, …) → [`Post`], by a
//!   config prefix list. They are MCP tools, not bash scripts, so they have clean names.
//! - **Irreversible capabilities are MCP tools** (`mcp__bankr__send`, `mcp__dac__vote`) →
//!   [`SettleTx`], by a config prefix list.
//! - **Raw shell** (Bash/PowerShell/REPL) → [`Shell`], denied everywhere (it would bypass
//!   `writable_dirs` path-gating).

use serde_json::Value;

use crate::config::CapabilityPrefix;
use crate::state::ToolClass;

/// Returns `(class, target_path)`. `target_path` is `Some` only for file-write tools,
/// extracted from the input so the responder can check it against `writable_dirs`.
pub fn classify_tool(
    tool: &str,
    input: &Value,
    settle_tools: &[CapabilityPrefix],
    post_tools: &[CapabilityPrefix],
    read_tools: &[CapabilityPrefix],
) -> (ToolClass, Option<String>) {
    // Config-driven capability tools win first, most-privileged first (settle > post > read), so
    // an irreversible prefix can never be shadowed by a looser one. Derived from the `mcp_servers`
    // registry tiers. The FIRST matching prefix CLAIMS the tool: if that server carries
    // a `tools` allowlist and the bare tool isn't on it, the tool is denied **fail-closed**
    // (→ `Other`), never re-classified by a looser prefix — this is how `cove-read` (read tier)
    // can serve an endpoint that also exposes `buy_token` without that trade tool ever counting
    // as Read. A non-whitelisted server (empty `tools`) admits its whole surface as before.
    for (prefixes, class) in [
        (settle_tools, ToolClass::SettleTx),
        (post_tools, ToolClass::Post),
        (read_tools, ToolClass::Read),
    ] {
        if let Some(cp) = prefixes.iter().find(|cp| tool.starts_with(cp.prefix.as_str())) {
            if !cp.tools.is_empty() {
                let bare = &tool[cp.prefix.len()..];
                if !cp.tools.iter().any(|t| t == bare) {
                    return (ToolClass::Other, None); // whitelisted server, tool not on the list.
                }
            }
            return (class, None);
        }
    }

    match tool {
        // Builtin FILE reads — blocked from the private `runlogs/` dir (the recall MCPs are the only
        // path to runlog history; the gitignore makes raw Glob/Read unreliable, so this denies it
        // outright → `Other`). A non-runlog path is a normal read.
        "Read" | "FileRead" | "Grep" | "Glob" | "LS" | "NotebookRead" => {
            if targets_runlogs(input) {
                return (ToolClass::Other, None);
            }
            (ToolClass::Read, None)
        }

        // NETWORK egress — reaches an arbitrary external URL / a search provider. Its OWN class
        // (`Net`), scoped to Perceive only (+ the sandboxed worker) — an exfil/SSRF/callback surface an
        // injected ACT cycle must never reach, so it is NOT lumped with local reads.
        "WebFetch" | "WebSearch" => (ToolClass::Net, None),

        // Non-file LOCAL/harness reads (no path to gate, no network egress).
        "LSP" | "ReadMcpResource" | "ListMcpResources" | "ToolSearch"
        | "TaskGet" | "TaskList" | "TaskOutput" => (ToolClass::Read, None),

        // File writes — the path decides memory vs soul (gated by writable_dirs).
        "Write" | "FileWrite" | "Edit" | "FileEdit" | "MultiEdit" | "NotebookEdit" => {
            (ToolClass::FileWrite, extract_path(input))
        }

        // Arbitrary execution — denied in every v1 state.
        "Bash" | "BashOutput" | "KillShell" | "PowerShell" | "REPL" => (ToolClass::Shell, None),

        // Unknown / Agent / Skill / other MCP → fail-closed.
        _ => (ToolClass::Other, None),
    }
}

/// Extract a write target path from a file-tool's input (`file_path`, then `path`).
fn extract_path(input: &Value) -> Option<String> {
    input
        .get("file_path")
        .or_else(|| input.get("path"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Does a file-read's input target the private `runlogs/` dir? Checks every path-ish field
/// (`file_path`, `path`, `pattern` — the last for `Glob`) for a `runlogs` path SEGMENT, so
/// `runlogs/2026-…md`, `…/runlogs`, and `runlogs/**/*` all match while `runlogs_archive` does not.
fn targets_runlogs(input: &Value) -> bool {
    ["file_path", "path", "pattern"].iter().any(|k| {
        input
            .get(*k)
            .and_then(Value::as_str)
            .is_some_and(|p| p.split(|c| c == '/' || c == '\\').any(|seg| seg == "runlogs"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const POST: &[&str] = &["mcp__twitter__"];
    const SETTLE: &[&str] = &["mcp__bankr__", "mcp__dac__", "mcp__cove-trading__"];
    const READ: &[&str] = &["mcp__cove-read__"];

    fn open(prefixes: &[&str]) -> Vec<CapabilityPrefix> {
        prefixes.iter().copied().map(CapabilityPrefix::open).collect()
    }
    fn post() -> Vec<CapabilityPrefix> {
        open(POST)
    }
    fn settle() -> Vec<CapabilityPrefix> {
        open(SETTLE)
    }
    fn read() -> Vec<CapabilityPrefix> {
        open(READ)
    }

    #[test]
    fn reads_and_writes_classify() {
        assert_eq!(classify_tool("Grep", &json!({}), &settle(), &post(), &read()).0, ToolClass::Read);
        let (c, p) = classify_tool("Write", &json!({"file_path": "memory/log.md"}), &settle(), &post(), &read());
        assert_eq!(c, ToolClass::FileWrite);
        assert_eq!(p.as_deref(), Some("memory/log.md"));
    }

    #[test]
    fn network_egress_is_its_own_net_class_not_read() {
        // WebFetch/WebSearch reach the outside network → `Net` (Perceive-only), NOT `Read` (every state).
        assert_eq!(classify_tool("WebFetch", &json!({"url": "http://x"}), &settle(), &post(), &read()).0, ToolClass::Net);
        assert_eq!(classify_tool("WebSearch", &json!({"query": "x"}), &settle(), &post(), &read()).0, ToolClass::Net);
        // A LOCAL non-file read stays Read.
        assert_eq!(classify_tool("ToolSearch", &json!({}), &settle(), &post(), &read()).0, ToolClass::Read);
    }

    #[test]
    fn capabilities_classify_by_prefix() {
        assert_eq!(classify_tool("mcp__twitter__post", &json!({}), &settle(), &post(), &read()).0, ToolClass::Post);
        assert_eq!(classify_tool("mcp__bankr__send", &json!({}), &settle(), &post(), &read()).0, ToolClass::SettleTx);
        // A registered monitoring MCP → Read (safe); the trading sibling → SettleTx (irreversible).
        assert_eq!(classify_tool("mcp__cove-read__price", &json!({}), &settle(), &post(), &read()).0, ToolClass::Read);
        assert_eq!(classify_tool("mcp__cove-trading__buy", &json!({}), &settle(), &post(), &read()).0, ToolClass::SettleTx);
    }

    #[test]
    fn twitter_read_and_write_prefixes_dont_collide() {
        // `twitter` (post) and `twitter-read` (read) share a stem but are DISTINCT prefixes — the
        // `-`/`__` boundary keeps them apart. Security-critical: a read tool must NEVER classify as
        // Post (it would let read-only Perceive "post"), and a post tool must never classify as Read
        // (it would escape Express). Holds regardless of prefix-check order.
        let post = open(&["mcp__twitter__"]);
        let read = open(&["mcp__twitter-read__", "mcp__cove-read__"]);
        let s = settle();
        assert_eq!(
            classify_tool("mcp__twitter-read__get_user", &json!({}), &s, &post, &read).0,
            ToolClass::Read,
        );
        assert_eq!(
            classify_tool("mcp__twitter__post", &json!({"text": "hi"}), &s, &post, &read).0,
            ToolClass::Post,
        );
        // the read tool is NOT a post; the post tool is NOT a read.
        assert_ne!(
            classify_tool("mcp__twitter-read__get_thread", &json!({}), &s, &post, &read).0,
            ToolClass::Post,
        );
        assert_ne!(
            classify_tool("mcp__twitter__reply", &json!({}), &s, &post, &read).0,
            ToolClass::Read,
        );
    }

    #[test]
    fn server_tool_allowlist_is_fail_closed() {
        // cove-read serves a full surface (incl. buy_token) on the read-only token; the operator's
        // `tools` allowlist holds it to read tools. A whitelisted read tool → Read; a non-listed
        // trade tool under the SAME prefix → Other (denied everywhere), NOT Read.
        let read = vec![CapabilityPrefix {
            prefix: "mcp__cove-read__".into(),
            tools: vec!["get_balance".into(), "scan_trending_tokens".into()],
        }];
        let post = post();
        let settle = settle();
        assert_eq!(
            classify_tool("mcp__cove-read__get_balance", &json!({}), &settle, &post, &read).0,
            ToolClass::Read
        );
        assert_eq!(
            classify_tool("mcp__cove-read__buy_token", &json!({}), &settle, &post, &read).0,
            ToolClass::Other // fail-closed: not on the allowlist → denied, never Read.
        );
    }

    #[test]
    fn runlog_file_reads_are_blocked_but_recall_mcp_and_normal_reads_pass() {
        let (s, p, r) = (settle(), post(), read());
        // Direct file access to a runlog → Other (denied everywhere). Glob by pattern + Read by path.
        assert_eq!(
            classify_tool("Glob", &json!({"path": "soul-template", "pattern": "runlogs/**/*"}), &s, &p, &r).0,
            ToolClass::Other,
        );
        assert_eq!(
            classify_tool("Read", &json!({"file_path": "/x/soul-template/runlogs/2026-06-29.md"}), &s, &p, &r).0,
            ToolClass::Other,
        );
        assert_eq!(
            classify_tool("Grep", &json!({"path": "runlogs"}), &s, &p, &r).0,
            ToolClass::Other,
        );
        // A normal read is unaffected; the `runlogs` SEGMENT must be exact (no false-match on a substring).
        assert_eq!(
            classify_tool("Read", &json!({"file_path": "memory/social.md"}), &s, &p, &r).0,
            ToolClass::Read,
        );
        assert_eq!(
            classify_tool("Glob", &json!({"pattern": "runlogs_archive/*"}), &s, &p, &r).0,
            ToolClass::Read,
        );
        // The recall MCPs read runlogs via node fs, NOT the Read tool — they classify by prefix, untouched.
        let recall = open(&["mcp__recall-self__", "mcp__recall__"]);
        assert_eq!(
            classify_tool("mcp__recall-self__recent_activity", &json!({}), &s, &p, &recall).0,
            ToolClass::Read,
        );
    }

    #[test]
    fn bash_is_shell_and_unknown_is_other() {
        assert_eq!(classify_tool("Bash", &json!({"command": "ls"}), &settle(), &post(), &read()).0, ToolClass::Shell);
        // Fail-closed: a tool we don't recognize is Other (→ default-denied).
        assert_eq!(classify_tool("SomeFutureTool", &json!({}), &settle(), &post(), &read()).0, ToolClass::Other);
    }
}
