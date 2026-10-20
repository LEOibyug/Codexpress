# Codexpress

Work in Simplified Chinese for user-facing copy and reports. Product: arm64 macOS SwiftUI menu-bar app with bundled Rust Responses gateway. Binding spec: docs/superpowers/specs/2026-10-20-codexpress-design.md. IPC contract: contracts/management.md.

- Do not read or modify real ~/.codex credentials/config/history, Keychain entries belonging to other apps, system proxy, Clash configuration, or reference repository files.
- Tests use temporary state directories and simulated localhost upstreams/proxies. No real paid generation or reset-card consumption.
- User requested reduced TDD ceremony. Test critical observable behaviors; do not add repetitive tests for simple structs or UI layout.
- Never log upstream secrets, request/response bodies, OAuth callback values or local gateway key.
- Keep work scoped to assigned files when collaborating. Do not overwrite another worker's edits. No subagents from worker agents.
- Source code must compile and important failures must be fixed before marking complete.
- References and local toolchain are ignored; never stage references/, .tools/, target/, .build/ or dist/.

