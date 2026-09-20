# Phase 3 Implementation Report: `self_verify` Gate Kind

## Status: COMPLETE

### Overview
Implemented the `self_verify` gate kind following the AutoMix / Self-REF pattern specified in SPEC §10.8 and the Reflexion Loop Implementation Plan Phase 3.

### Changes Made

1. **`crates/firstpass-core/src/config.rs`:**
   - Added `GateKind` enum with variants: `Subprocess`, `Judge`, `Consistency`, `Schema`, and `SelfVerify { pass_when: String, prompt: Option<String> }`.
   - Added `SelfVerifyDef` struct with `pass_when: String` (defaults to `"high"`) and `prompt: Option<String>`.
   - Added `default_pass_when() -> String` helper function.
   - Added `kind: Option<String>`, `pass_when: Option<String>`, `prompt: Option<String>`, `self_verify: Option<SelfVerifyDef>` fields to `GateDef`.
   - Implemented `Default for GateDef` to allow `..Default::default()` across all construction sites.
   - Implemented `GateDef::kind(&self) -> Option<GateKind>` helper to resolve the effective gate kind cleanly.
   - Added gate validation in `Config::parse`: enforces exactly one gate kind among `cmd`, `judge`, `consistency`, `schema`, and `self_verify`; validates non-empty `pass_when`; rejects unknown `kind`; rejects `pass_when`/`prompt` set on non-`self_verify` gates.
   - Added unit tests:
     - `parses_self_verify_gate_definition`
     - `parses_self_verify_gate_defaults`
     - `parses_self_verify_gate_table_syntax`
     - `rejects_self_verify_violations`

2. **`crates/firstpass-core/src/lib.rs`:**
   - Re-exported `GateKind` and `SelfVerifyDef` from `firstpass_core::config`.

3. **`crates/firstpass-proxy/src/gate.rs`:**
   - Defined `DEFAULT_SELF_VERIFY_PROMPT` (`"Rate your confidence in the above response."`).
   - Defined `SELF_VERIFY_SYSTEM_PROMPT` (`"Answer only with a single word: high, medium, or low."`).
   - Implemented `SelfVerifyGate` struct and `Gate` trait implementation:
     - Resolves provider from `req.model` via `registry`.
     - Calls same model that produced the executor candidate response.
     - Sets `max_tokens: 15`.
     - Sets pinned `system` prompt: `"Answer only with a single word: high, medium, or low."`.
     - Sets user message with response text and prompt: `format!("{}\n\n{}", resp.text, prompt_text)`.
     - Evaluates whether model output contains `pass_when` (case-insensitive substring check on trimmed text).
     - Returns `GateResult` with `Verdict::Pass` or `Verdict::Fail` and descriptive reason.
     - Records verification cost in `cost_usd` via `PriceTable::cost_usd_with_cache`.
     - Returns `Verdict::Abstain` if provider is missing or call fails.
   - Updated `resolve_gates` to match on `def.kind()` and construct `SelfVerifyGate` when `GateKind::SelfVerify` is present.
   - Updated existing tests in `gate.rs` to include `..Default::default()` on `GateDef` literals.
   - Added unit tests:
     - `self_verify_gate_config_parses`
     - `self_verify_passes_on_matching_response`
     - `self_verify_fails_on_non_matching_response`
     - `self_verify_fails_on_medium`
     - `self_verify_abstains_on_missing_provider`

### Verification
- `cargo test -p firstpass-core -- gate`: 10 passed, 0 failed.
- `cargo test -p firstpass-proxy -- gate`: 34 passed, 0 failed.
- `cargo clippy --workspace --all-targets -- -D warnings`: 0 warnings, 0 errors.
- `cargo fmt --all --check`: Clean formatting across workspace.
- `cargo test --workspace`: 443 proxy tests, 153 core tests, 9 end-to-end tests, 3 observe-loop tests all passed.
