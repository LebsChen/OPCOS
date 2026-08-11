# OPCOS development guide

## Local verification

```bash
cargo build
cargo test
cargo clippy --workspace --all-targets -- -D warnings
(cd web && npm install && npx tsc --noEmit && npm run build)
(cd web && npm run format:check)
```

## Hard constraints

1. The RVM host side is unchanged; OPCOS is client-only.
2. RVM tokens are sent only in `Authorization: Bearer` headers, except for the
   remote IDE front-door bootstrap: the host requires `/ide/?tkn=...` and
   rejects bearer-only IDE requests with 403, so that sanctioned URL is used
   only to obtain IDE cookies. All other `/api/*` calls, logs, errors,
   transcripts, fixtures, and UI remain header-only and must never expose token
   values.
3. A unavailable remote host returns an explicit error; never silently fall back
   to local execution.
4. Remote paths use remote path algebra and containment checks, never local
   `Path::canonicalize`.
5. The frontend uses Tauri invoke and event channels, not a Python HTTP
   sidecar.

## Layering

- `opcos-rvm` does not depend on `opcos-engine`.
- `opcos-engine` does not depend on Tauri or frontend code.
- Cross-layer behavior is expressed through traits.
- `src-tauri` is the desktop adapter, not the agent runtime.

Current stable Rust is required. Do not pin Rust 1.83.
