`codex-models-0.162.1.json` is the unmodified model catalog from OpenAI Codex
tag `rust-v0.162.1`, `codex-rs/models-manager/models.json`;
`codex-prompt-0.162.1.md` is its fallback `codex-rs/models-manager/prompt.md`:
https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/models-manager/models.json

Licensed under Apache-2.0; the upstream license and notice are included here
and in the application bundle. This offline fallback is used only when applying
per-model compaction rules without an existing custom catalog or model cache.
Existing model instructions, capabilities and unconfigured defaults are retained.
The generated catalog is static until the user applies rules again.
