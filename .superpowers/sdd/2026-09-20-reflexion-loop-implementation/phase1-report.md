# Phase 1 Implementation Report — Core Schema & Config

- **Phase:** Phase 1 — Core schema & config (`firstpass-core`)
- **Status:** Complete
- **Date:** 2026-09-20

## Summary of Changes

All requested changes were made strictly within `crates/firstpass-core` adhering to all invariants (no I/O, no network, no new crate dependencies, strict backward compatibility for canonical JSON and hash chains).

### 1. `crates/firstpass-core/src/features.rs`
- Added `subagent_name: Option<String>` to `Features` with `#[serde(default, skip_serializing_if = "Option::is_none")]`.
- Updated constructor `Features::new` to initialize `subagent_name: None`.
- Added unit test `subagent_name_roundtrip_and_backward_compat` verifying backward compatibility when `None` and proper wire serialization as `"subagent_name"` when `Some(...)`.

### 2. `crates/firstpass-core/src/config.rs`
- Added `ReflexionExhaustedPolicy` enum with variants `ServeBestAttempt` (default) and `Error`, derived with `Debug, Clone, Deserialize, Default, PartialEq, Eq`, and annotated with `#[serde(rename_all = "snake_case")]`.
- Added `ReflexionConfig` struct with `#[serde(deny_unknown_fields)]` and exact field ordering:
  - `mentor_model: String`
  - `max_reflections: u32` (default = 2 via `default_max_reflections`)
  - `mentor_max_out_tokens: u32` (default = 200 via `default_mentor_max_out_tokens`)
  - `mentor_system_prompt: Option<String>`
  - `inject_as_user_turn: bool` (default = false)
  - `convergence_threshold: f64` (default = 0.0)
  - `max_latency_ms: Option<u64>` (default = None)
  - `on_reflexion_exhausted: ReflexionExhaustedPolicy` (default = `ServeBestAttempt` via `default_on_reflexion_exhausted`)
- Added `validate(&self) -> Result<()>` on `ReflexionConfig` ensuring:
  - `max_reflections` in `[1, 5]`
  - `mentor_max_out_tokens > 0`
  - `convergence_threshold` in `[0.0, 1.0)`
- Added `#[serde(default)] pub reflexion: Option<ReflexionConfig>` to both `Config` and `Route` structs (handling `deny_unknown_fields`).
- Wired validation in `Config::parse` for both `config.reflexion` and each `route.reflexion`.
- Added unit tests `reflexion_config_parses_defaults_and_custom_values` and `reflexion_config_validates_invariants`.

### 3. `crates/firstpass-core/src/trace.rs`
- Added to `Attempt`:
  - `reflexion_cycle: Option<u32>`
  - `mentor_correction_hash: Option<String>`
  - `reflexion_converged: Option<bool>`
  (all with `#[serde(default, skip_serializing_if = "Option::is_none")]`)
- Added to `FinalOutcome`:
  - `reflexion_cycles: Option<u32>`
  - `mentor_cost_usd: Option<f64>`
  - `reflexion_cycles_to_pass: Option<u32>`
  - `triggered_by_self_verify: Option<bool>`
  - `reflexion_latency_capped: Option<bool>`
  (all with `#[serde(default, skip_serializing_if = "Option::is_none")]`)
- Updated test helper `sample_trace` with `None` defaults for new fields.
- Added unit tests:
  - `reflexion_fields_absent_when_none_preserves_backward_compat`: confirms that an `Attempt`, `FinalOutcome`, or full `Trace` with `None` fields serializes without any `reflexion`, `mentor_`, or `triggered_by_self_verify` keys in the JSON output, preserving the hash chain and audit re-derivation invariants.
  - `reflexion_fields_roundtrip_when_present`: confirms serialization and deserialization round-trip when reflexion fields are populated.

### 4. `crates/firstpass-core/src/lib.rs`
- Re-exported `ReflexionConfig` and `ReflexionExhaustedPolicy` from `config`.

## Verification Results

- `cargo test -p firstpass-core`: 149 passed; 0 failed (5 new tests added, all passed).
- `cargo clippy -p firstpass-core --all-targets -- -D warnings`: Clean, 0 warnings.
- `cargo fmt --all --check`: Clean formatting across the workspace.

## Notes & Considerations for Phase 2

- In `firstpass-proxy`, Phase 2 will introduce `reflexion.rs` containing `ReflexionCtx` and `run_reflexion_loop`.
- Phase 2 can import `ReflexionConfig` and `ReflexionExhaustedPolicy` directly from `firstpass_core::config::{ReflexionConfig, ReflexionExhaustedPolicy}` (or `firstpass_core::{ReflexionConfig, ReflexionExhaustedPolicy}`).
- Hash chain integrity remains protected because all new trace fields on `Attempt` and `FinalOutcome` are omitted when `None`.

## Fix Round 1 Report (C1 — Proxy Struct Literals)

- **Issue:** Adding public fields to `Attempt`, `FinalOutcome`, and `Route` broke struct literal instantiations across `crates/firstpass-proxy` (11 library errors, 37 test errors).
- **Remediation:**
  - `crates/firstpass-proxy/src/router.rs`: Added `None` for new fields in `FinalOutcome` (`make_outcome_and_trace`), `Attempt` in ladder execution, and `abstain_attempt`.
  - `crates/firstpass-proxy/src/proxy.rs`: Added `None` for new fields in `FinalOutcome` (cache lookup, `build_observe_trace`, `build_stream_trace`, `build_error_trace`, `base_trace`), `Attempt` (observe trace and mock test trace), and `Route` (`bare_enforce_route`).
  - `crates/firstpass-proxy/src/bandit.rs`: Added `None` for `Attempt` in `stub_attempt` and `FinalOutcome` in test trace constructor.
  - `crates/firstpass-proxy/src/calibrate.rs`: Added `None` for `Attempt` and `FinalOutcome` in `trace_with_score`.
  - `crates/firstpass-proxy/src/cli.rs`: Added `None` for `Attempt` and `FinalOutcome` across export and explain unit tests.
  - `crates/firstpass-proxy/src/ope.rs`: Added `None` for `Attempt` and `FinalOutcome` in `make_test_trace`, `make_escalated_trace`, and `make_propensity_trace`.
  - `crates/firstpass-proxy/src/store.rs`: Added `None` for `Attempt` and `FinalOutcome` in test helper `sample_trace`; resolved `while_let_loop` lint in trace writer loop.
  - `crates/firstpass-proxy/src/verified_cache.rs`: Added `None` for `Attempt` and `FinalOutcome` in test helper.

### Verification Results After Fix
- `cargo test --workspace`: 782 passed; 0 failed.
- `cargo clippy --workspace --all-targets -- -D warnings`: Clean, 0 warnings across all workspace crates and test targets.
- `cargo fmt --all --check`: Clean formatting across the workspace.

