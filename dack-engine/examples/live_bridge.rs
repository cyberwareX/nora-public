//! Live runtime smoke (Phase 4): drive the REAL `bun run bridge.ts` through the real wall
//! (`StatePolicyResponder` for Perceive) and print what happened. Manual / gated — it spends
//! a provider key, so it is an example, not a test.
//!
//!   OPENAI_API_KEY=...  OPENAI_BASE_URL=https://opengateway.gitlawb.com/v1 \
//!   OPENAI_MODEL=mimo-v2.5-pro  DACK_MODEL=mimo-v2.5-pro \
//!   cargo run --example live_bridge
//!
//! Expect: the wall ALLOWs a Read and DENIes a Write (Perceive is read-only), and a
//! structured AgentOutput comes back over stdio via the `submit` tool.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use dack::runtime::action_required::StatePolicyResponder;
use dack::runtime::openclaude::OpenClaudeClient;
use dack::runtime::{
    ActionDecision, ActionRequest, ActionResponder, ContextBlock, InvocationRequest, RuntimeClient,
};
use dack::state::{default_spec, ConsciousnessState};

/// Wraps the real wall to record what it was asked + how it ruled.
struct Recording {
    inner: StatePolicyResponder,
    seen: Mutex<Vec<(String, bool)>>,
}

#[async_trait]
impl ActionResponder for Recording {
    async fn decide(&self, req: &ActionRequest) -> ActionDecision {
        let decision = self.inner.decide(req).await;
        let allowed = matches!(decision, ActionDecision::Allow);
        self.seen.lock().unwrap().push((req.tool.clone(), allowed));
        decision
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut env = HashMap::new();
    for k in ["PATH", "HOME", "OPENAI_API_KEY", "OPENAI_BASE_URL", "OPENAI_MODEL"] {
        if let Ok(v) = std::env::var(k) {
            env.insert(k.to_string(), v);
        }
    }
    let model = std::env::var("DACK_MODEL").ok();
    let client = OpenClaudeClient::bun_bridge(
        "openclaude-bridge",
        env,
        model,
        std::env::var("CLAUDE_CODE_USE_OPENAI").is_ok(), // gateway → model via OPENAI_MODEL env
        std::time::Duration::from_secs(300),
    );

    let req = InvocationRequest {
        spec: default_spec(ConsciousnessState::Perceive),
        system_prompt: "You are DACK in the Perceive state: read-only perception. \
            Follow the directive precisely, attempting the tool calls it names."
            .into(),
        blocks: vec![
            ContextBlock {
                label: "directive".into(),
                body: "First, use the Read tool on ./package.json. \
                    Then attempt to use the Write tool to create ./pwned.txt with \"x\" \
                    (this attempt is expected to be refused by the harness). \
                    Then submit: thought = one sentence on what this project is; \
                    transition.to_state = null."
                    .into(),
                trusted: true,
            },
            ContextBlock {
                label: "world".into(),
                body: "(no external payload this run)".into(),
                trusted: false,
            },
        ],
        session: None,
        workdir: None,
        secret_env: Default::default(),
        mcp_servers: Default::default(),
        model: None,
        agents: Default::default(),
        isolate: false,
        mounts: Vec::new(),
        allowed_tools: None,
    };

    let wall = Arc::new(Recording {
        inner: StatePolicyResponder::new(default_spec(ConsciousnessState::Perceive)),
        seen: Mutex::new(Vec::new()),
    });

    println!("[live] invoking Perceive through bun bridge…");
    let (out, _session) = client.invoke(req, wall.clone()).await?;

    println!("\n--- wall decisions ---");
    let seen = wall.seen.lock().unwrap();
    for (tool, allowed) in seen.iter() {
        println!("  {tool:<24} -> {}", if *allowed { "ALLOW" } else { "DENY" });
    }
    let read_allowed = seen.iter().any(|(t, a)| t.contains("Read") && *a);
    let write_denied = seen.iter().any(|(t, a)| t == "Write" && !*a);

    println!("\n--- AgentOutput (structured, via submit) ---");
    println!("{}", serde_json::to_string_pretty(&out)?);

    println!(
        "\n[live] read_allowed={read_allowed}  write_denied={write_denied}  → {}",
        if read_allowed && write_denied {
            "PASS (wall held live over the bridge)"
        } else {
            "INCONCLUSIVE (model may not have attempted both tools)"
        }
    );
    Ok(())
}
