# Codexpress Implementation Plan

> **For agentic workers:** Use superpowers:subagent-driven-development for focused implementation and review. Independent file scopes may run concurrently using dispatching-parallel-agents. The user's later instruction reduces TDD ceremony: test critical behavioral boundaries, do not write tests for every function or UI detail.

**Goal:** Ship a usable arm64 native menu-bar app and local Responses gateway with per-source proxy isolation, mixed priority routing, official OAuth/usage/manual reset credits, and reversible Codex configuration attachment.

**Architecture:** SwiftUI owns UI, Keychain and helper lifecycle. A bundled Rust process owns all upstream networking, SQLite usage and source routing. JSON requests over an owner-only Unix socket form the management API; Codex uses an authenticated loopback HTTP endpoint.

**Tech Stack:** SwiftUI/AppKit/Security, SwiftPM, Rust/Tokio/Axum/reqwest/toml_edit/rusqlite.

**Spec:** `/Users/rhetoric/Work/Codexpress/docs/superpowers/specs/2026-10-20-codexpress-design.md`

## Global Constraints

- Official OAuth and API sources share the priority pool. Clash TUN remains off.
- No live user authentication/configuration changes during development or tests.
- Native Responses JSON/SSE forwarding; no Chat/Anthropic conversion. Initial managed provider disables WebSocket.
- Per-source direct or explicit proxy, never implicit fallback. Official sources require explicit proxy.
- Only a local gateway credential goes in Codex TOML; real upstream secrets remain in Keychain/runtime memory.
- Manual reset-card requests bind source/card and stable operation ID, never auto-consume or blindly replay uncertain submissions.
- Use current checkout: this is a new repository without an existing branch to protect. References stay ignored and unchanged.
- Build local arm64 macOS app. Signing is local ad-hoc; notarization is not claimed.

## Shared contracts and file ownership

- `contracts/management.md` defines JSON models and Unix-socket operations; all implementers read this first.
- Root owns `gateway/src/model.rs`, `gateway/src/management.rs`, `gateway/src/main.rs`, config attachment, workspace build files, integration tests, packaging and docs.
- Gateway implementation task owns `gateway/src/relay.rs`, `gateway/src/routing.rs`, `gateway/src/transport.rs`, `gateway/src/usage.rs`, and its tests.
- Official implementation task owns `gateway/src/official.rs`, OAuth/reset tests; it consumes transport client factory and model types.
- Swift task owns `Package.swift`, `Sources/Codexpress/**`, Swift test target if needed. Its IPC client uses the frozen management contract.

### Task 1: Contracts, state, reversible attachment

**Files:** `contracts/management.md`, `gateway/Cargo.toml`, `gateway/src/{model,config,management,main}.rs`, `gateway/tests/config.rs`.

**Interfaces:** `AppState` holds source credentials in memory, routing health, persistent usage and settings. `config::preview/attach/restore` accepts an explicit CODEX_HOME and snapshot path. Management op envelope is `{id, op, payload}` and response is `{id, ok, data, error}`.

- [x] Write the contract and shared Rust models before dispatching independent implementation.
- [x] Implement TOML patching with snapshot, content comparison, atomic 0600 writes and selective restore. Reject conflicting managed changes.
- [x] Validate with `cargo test --manifest-path gateway/Cargo.toml --test config`: fixture includes nested MCP, comments and unknown settings; attach then external unrelated edit then restore must retain both old settings and new edit. Refuse a conflicting provider edit. No test uses real HOME.
- [x] Commit the baseline and configuration protection changes.

### Task 2: Relay, source policy and usage

**Files:** `gateway/src/{relay,routing,transport,usage}.rs`, focused gateway tests.

**Interfaces:** `transport::client(&Source) -> Result<reqwest::Client>` enforces source egress. `relay::router(Arc<AppState>) -> axum::Router` exposes authenticated loopback `/v1/responses`, compact and model discovery. `usage::UsageStore` persists metadata; `routing` selects eligible sources with sticky binding and fixed-source semantics.

- [x] Implement original-body forwarding, identity-header replacement, streaming with bounded usage observation, cancel propagation and no cross-source replay once output begins.
- [x] Implement ordered candidate selection, explicit exhausted/auth/fault states and model filtering. Route request-bound server state to its originating source.
- [x] Add local upstream/proxy tests that detect byte mutation, incorrect credentials, bypassed proxy, wrong fallback and cached-input double counting.
- [x] Run scoped tests, inspect diff, commit only owned files; task review follows.

### Task 3: Official OAuth, quotas and manual reset cards

**Files:** `gateway/src/official.rs`, `gateway/tests/official.rs`.

**Interfaces:** official endpoints are dispatched by `official::handle(&Arc<AppState>, op, payload)`. Sources carry secret JSON for OAuth tokens and account identity. Token updates are returned through `sync` so Swift persists them into Keychain. The module uses the same `transport::client` factory as generation.

- [x] Implement PKCE/state and localhost callback, with manual complete-callback fallback; strictly reject invalid/missing state. Exchange and refresh tokens through source proxy. Serialize refresh per account.
- [x] Query official usage and reset cards, preserve unknown values and stale state. Use conservative error classification.
- [x] Manual consume requires explicit `confirmed=true`, source/card/operation ID, checks card state, serializes per source and persists uncertain operation metadata before dispatch. No automatic replay. Successful result triggers quota/card refresh and clears only restored exhaustion state.
- [x] Test parsing, wrong-source/state rejection and duplicate/uncertain consume behavior with controlled local network fixtures; commit only owned files.

### Task 4: Native menu-bar client and credentials

**Files:** `Package.swift`, `Sources/Codexpress/{App,Models,GatewayClient,AppModel,KeychainStore,MenuView,SettingsView,SourceEditor,UsageView,ResetCreditsView}.swift`.

**Interfaces:** Management contract is the sole helper API. Swift reads source metadata from app support JSON and credentials from Keychain, sends sources to helper via `sync`. Helper stderr goes to a protected bounded log. Startup passes app support directory and configured port.

- [x] Implement native menu-bar popover and four-page settings, empty states, draggable source order, fixed/auto route mode, per-source direct/proxy controls, OAuth add/complete, quota/card refresh and manual confirmation.
- [x] Implement Keychain storage, credential update persistence and source metadata persistence with 0600 permissions. Never serialize secrets to metadata JSON or show secrets in logs.
- [x] Implement preview/attach/restore UI, gateway lifecycle, source proxy errors and stale/unknown quota display. Existing-history routing boundary is visible in attachment preview.
- [x] Run `swift build`; inspect launched UI with native computer-use tools. Test only nontrivial persistence/IPC behavior if useful; avoid tests for layout constants.
- [x] Commit only owned files; task review follows.

### Task 5: End-to-end integration, packaging and review

**Files:** `gateway/src/management.rs`, `scripts/{build,run,check}.sh`, `Resources/Info.plist`, `README.md`, targeted integration tests.

- [x] Connect all modules and test helper over Unix socket with simulated sources and isolated Codex home.
- [x] Run `cargo test --manifest-path gateway/Cargo.toml`, `cargo clippy --manifest-path gateway/Cargo.toml --all-targets -- -D warnings`, `swift build -c release` and build script; fix meaningful failures.
- [x] Package `dist/Codexpress.app` with helper and app metadata, ad-hoc sign both, verify arm64 executable and signature. Launch empty-state UI without changing live Codex settings; capture/inspect UI.
- [x] Perform final code review for credentials, proxy fallback, config restore, stream buffering and reset ambiguity. Resolve material findings and rerun covering checks.
- [x] Document real-login validation still requiring user's account, exact supported transport scope and build/run instructions. Update spec status and deliver app plus evidence.

## Execution record

2026-10-20: User approved implementation and requested less TDD. Tests target critical regressions; grouped implementation and verification replaces repetitive red/green steps. No additional design permission needed.

2026-10-20 完成记录：本地源码、arm64 app、39项关键测试、Clippy及Codex CLI隔离端到端通过；独立审查的重要问题已修复。任务5的目标客户端验收完成CLI与本应用原生界面；真实Codex Desktop旧会话、真实OAuth/供应商/重置权益由用户配置后验收，不计为已实测。详见 docs/verification-2026-10-20.md。
