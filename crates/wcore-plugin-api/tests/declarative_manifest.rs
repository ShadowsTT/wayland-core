//! Path B step 1 — declarative on-disk plugin manifest schema tests.
//!
//! A declarative plugin ships a `plugin.toml` (no executable) that contributes
//! `[[hooks]]` + an optional `[mcp_server]` block. These tests pin the parse
//! + validation contract for that manifest shape.

use wcore_plugin_api::error::PluginError;
use wcore_plugin_api::manifest::PluginManifest;
use wcore_plugin_api::mcp_server_spec::McpTransport;
use wcore_plugin_api::registry::hooks::HookPhase;

/// T1 — `[[hooks]]` parse: phase + tool round-trip through the snake_case
/// `HookPhase` enum.
#[test]
fn declarative_hooks_parse() {
    let toml = r#"
[plugin]
name = "declarative-hooks"
version = "0.1.0"
description = "declarative hooks"
license = "MIT"

[permissions]
register_hooks = true

[[hooks]]
phase = "session_start"
tool = "memory_prelude"

[[hooks]]
phase = "pre_tool_use"
tool = "guard_tool"
"#;
    let m = PluginManifest::from_toml_str(toml).expect("parse declarative hooks");
    assert_eq!(m.hooks.len(), 2);
    assert_eq!(m.hooks[0].phase, HookPhase::SessionStart);
    assert_eq!(m.hooks[0].tool, "memory_prelude");
    assert_eq!(m.hooks[1].phase, HookPhase::PreToolUse);
    assert_eq!(m.hooks[1].tool, "guard_tool");
}

/// T2 — `[mcp_server]` parse: an `McpServerSpec` stdio transport.
#[test]
fn declarative_mcp_server_parse() {
    let toml = r#"
[plugin]
name = "declarative-mcp"
version = "0.1.0"
description = "declarative mcp"
license = "MIT"

[permissions]
register_mcp_server = true

[mcp_server]
name = "my-memory"

[mcp_server.transport]
kind = "stdio"
command = "npx"
args = ["-y", "@me/memory-server"]
"#;
    let m = PluginManifest::from_toml_str(toml).expect("parse declarative mcp_server");
    let spec = m.mcp_server.expect("mcp_server present");
    assert_eq!(spec.name, "my-memory");
    match spec.transport {
        McpTransport::Stdio { command, args } => {
            assert_eq!(command, "npx");
            assert!(args.iter().any(|a| a == "@me/memory-server"));
        }
        other => panic!("expected stdio transport, got {other:?}"),
    }
}

/// T3 — `[[hooks]]` present but `register_hooks` not granted → ManifestSchema.
#[test]
fn declarative_hooks_require_permission() {
    let toml = r#"
[plugin]
name = "declarative-hooks-nogrant"
version = "0.1.0"
description = "hooks without grant"
license = "MIT"

[[hooks]]
phase = "session_start"
tool = "memory_prelude"
"#;
    let err = PluginManifest::from_toml_str(toml).expect_err("must reject hooks without grant");
    assert!(
        matches!(err, PluginError::ManifestSchema { .. }),
        "expected ManifestSchema, got {err:?}"
    );
}

/// T4 — `[mcp_server]` present but `register_mcp_server` not granted → err.
#[test]
fn declarative_mcp_server_requires_permission() {
    let toml = r#"
[plugin]
name = "declarative-mcp-nogrant"
version = "0.1.0"
description = "mcp without grant"
license = "MIT"

[mcp_server]
name = "my-memory"

[mcp_server.transport]
kind = "stdio"
command = "npx"
args = []
"#;
    let err =
        PluginManifest::from_toml_str(toml).expect_err("must reject mcp_server without grant");
    assert!(
        matches!(err, PluginError::ManifestSchema { .. }),
        "expected ManifestSchema, got {err:?}"
    );
}

/// T5 — `kind = "declarative"` accepted; an unknown kind still rejected.
#[test]
fn declarative_runtime_kind_accepted() {
    let toml = r#"
[plugin]
name = "declarative-kind"
version = "0.1.0"
description = "declarative kind"
license = "MIT"

[runtime]
kind = "declarative"
"#;
    let m = PluginManifest::from_toml_str(toml).expect("declarative kind must parse");
    assert_eq!(
        m.runtime.as_ref().map(|r| r.kind.as_str()),
        Some("declarative")
    );
}

#[test]
fn unknown_runtime_kind_still_rejected() {
    let toml = r#"
[plugin]
name = "declarative-bad-kind"
version = "0.1.0"
description = "bad kind"
license = "MIT"

[runtime]
kind = "totally-bogus"
"#;
    let err = PluginManifest::from_toml_str(toml).expect_err("unknown kind must reject");
    assert!(
        matches!(err, PluginError::UnknownRuntimeKind { .. }),
        "expected UnknownRuntimeKind, got {err:?}"
    );
}

/// T6 — a manifest with NO `entry` parses (declarative plugins have no binary).
#[test]
fn manifest_without_entry_parses() {
    let toml = r#"
[plugin]
name = "no-entry"
version = "0.1.0"
description = "no entry"
license = "MIT"
"#;
    let m = PluginManifest::from_toml_str(toml).expect("manifest without entry must parse");
    assert_eq!(m.plugin.entry, None);
}

/// T7 — an existing manifest WITH `entry` still parses (backward compatible).
#[test]
fn manifest_with_entry_round_trips() {
    let toml = r#"
[plugin]
name = "with-entry"
version = "0.1.0"
description = "has entry"
entry = "builtin:with_entry"
license = "MIT"
"#;
    let m = PluginManifest::from_toml_str(toml).expect("manifest with entry must parse");
    assert_eq!(m.plugin.entry.as_deref(), Some("builtin:with_entry"));
}

/// Hooks-without-mcp_server is explicitly allowed (only the permission gate
/// applies). Guards against an over-eager validate that couples the two.
#[test]
fn declarative_hooks_without_mcp_server_allowed() {
    let toml = r#"
[plugin]
name = "hooks-only"
version = "0.1.0"
description = "hooks only"
license = "MIT"

[permissions]
register_hooks = true

[[hooks]]
phase = "turn_end"
tool = "summarize"
"#;
    let m = PluginManifest::from_toml_str(toml).expect("hooks-only declarative plugin must parse");
    assert_eq!(m.hooks.len(), 1);
    assert!(m.mcp_server.is_none());
}
