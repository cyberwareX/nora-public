//! CWD-fix validation (2026-06-16): drive the REAL `bun run bridge.ts` with a WORKER-style invoke
//! (worker_spec, workdir = a fresh temp workspace) and a directive to write a RELATIVE file. Asserts
//! the file lands in the WORKSPACE — not in `openclaude-bridge/` (the bridge's own dir), which was
//! the bug: the SDK resolves tool cwd from process.cwd(), not options.cwd, so a worker's relative
//! writes leaked into the bridge dir. The fix is `process.chdir(inv.cwd)` in the bridge. Manual /
//! gated (spends a provider key):
//!
//!   CLAUDE_CODE_USE_OPENAI=1 OPENAI_API_KEY=ogw_live_… \
//!   OPENAI_BASE_URL=https://opengateway.gitlawb.com/v1 \
//!   OPENAI_MODEL=xiaomi/mimo-v2.5-pro DACK_MODEL=xiaomi/mimo-v2.5-pro \
//!   cargo run --example cwd_check

use std::collections::HashMap;
use std::sync::Arc;

use dack::runtime::action_required::StatePolicyResponder;
use dack::runtime::openclaude::OpenClaudeClient;
use dack::runtime::{ContextBlock, InvocationRequest, RuntimeClient};
use dack::state::worker_spec;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut env = HashMap::new();
    for k in ["PATH", "HOME", "OPENAI_API_KEY", "OPENAI_BASE_URL", "OPENAI_MODEL", "CLAUDE_CODE_USE_OPENAI"] {
        if let Ok(v) = std::env::var(k) {
            env.insert(k.to_string(), v);
        }
    }
    let model = std::env::var("DACK_MODEL").ok();
    let client = OpenClaudeClient::bun_bridge(
        "openclaude-bridge",
        env,
        model,
        std::env::var("CLAUDE_CODE_USE_OPENAI").is_ok(),
        std::time::Duration::from_secs(300),
    );

    // A fresh workspace, exactly like run_worker_detached makes.
    let workspace = std::env::temp_dir().join(format!("dack-cwd-check-{}", std::process::id()));
    std::fs::create_dir_all(&workspace)?;
    let bridge_stray = std::path::Path::new("openclaude-bridge/solution.py");
    let _ = std::fs::remove_file(bridge_stray); // start clean so a hit is unambiguous

    println!("[cwd] workspace = {}", workspace.display());

    let req = InvocationRequest {
        spec: worker_spec(),
        system_prompt: "You are a sandboxed coding worker. Do EXACTLY what the directive says, \
            then submit your result. Keep it to a single file."
            .into(),
        blocks: vec![ContextBlock {
            label: "directive".into(),
            body: "Use the BASH tool to create a file named exactly `solution.py` — a RELATIVE \
                path in your current working directory, NOT an absolute path — by running:\n\
                printf 'def reverse(s):\\n    return s[::-1]\\n' > solution.py\n\
                Then run `python3 solution.py` (it should exit 0). This exercises BOTH the worker's \
                cwd and its shell. Then submit: thought = a short note on whether bash worked; \
                transition.to_state = null."
                .into(),
            trusted: true,
        }],
        session: None,
        workdir: Some(workspace.clone()),
        secret_env: Default::default(),
        mcp_servers: Default::default(),
        model: None,
        agents: Default::default(),
        isolate: false,
        mounts: Vec::new(),
        allowed_tools: None,
    };

    // The real worker wall: worker_spec, relativize root = the workspace.
    let wall = Arc::new(StatePolicyResponder::new(worker_spec()).with_repo_root(workspace.clone()));

    println!("[cwd] invoking worker through bun bridge…");
    let (out, _session) = client.invoke(req, wall).await?;
    println!("[cwd] worker thought: {}", out.thought);

    let in_workspace = workspace.join("solution.py").exists();
    let in_bridge = bridge_stray.exists();
    let _ = std::fs::remove_file(bridge_stray); // tidy up if the bug recurred

    println!("\n[cwd] solution.py in WORKSPACE = {in_workspace}");
    println!("[cwd] solution.py in openclaude-bridge/ = {in_bridge}  (must be false)");
    println!(
        "[cwd] → {}",
        if in_workspace && !in_bridge {
            "PASS — relative write landed in the worker's workspace (cwd fix holds)"
        } else if in_bridge {
            "FAIL — write leaked into the bridge dir (cwd bug NOT fixed)"
        } else {
            "INCONCLUSIVE — model didn't write a relative solution.py (retry)"
        }
    );
    Ok(())
}
