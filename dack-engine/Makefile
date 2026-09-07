# DACK — a Rust harness (`dack`) + a small TS runtime bridge (`openclaude-bridge/`).
# Two languages, one tool: the harness spawns the bridge per state invocation over stdio.
SHELL := /bin/bash
BRIDGE := openclaude-bridge

.PHONY: all build bridge test smoke run fmt lint clean dist help

help: ## Show this help
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | sed -E 's/:.*## /\t/' | sort

all: build

build: bridge ## Release Rust binary + install the bridge's npm deps
	cargo build --release

bridge: ## Install the runtime bridge's deps (@gitlawb/openclaude from npm)
	cd $(BRIDGE) && bun install --frozen-lockfile

test: ## Rust test suite (offline, deterministic)
	cargo test

smoke: ## Live runtime smoke — needs OPENAI_API_KEY / OPENAI_BASE_URL / OPENAI_MODEL
	cargo run --example live_bridge

run: ## Boot the harness (ingestion + consciousness loops)
	cargo run --release -- run

fmt: ## Format Rust
	cargo fmt

lint: ## Clippy, warnings-as-errors
	cargo clippy --all-targets -- -D warnings

clean: ## Remove build artifacts + bridge deps + dist
	cargo clean
	rm -rf $(BRIDGE)/node_modules dist

dist: build ## Assemble a deployable bundle (binary + bridge project)
	rm -rf dist && mkdir -p dist/$(BRIDGE)
	cp target/release/dack dist/
	cp $(BRIDGE)/bridge.ts $(BRIDGE)/package.json $(BRIDGE)/bun.lock dist/$(BRIDGE)/
	cp dack.config.example.yaml dist/
	@echo "dist/ ready: ./dack + $(BRIDGE)/ — on the box run 'cd $(BRIDGE) && bun install --frozen-lockfile'"
