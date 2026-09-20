# Phase 4 & Phase 5 Report: Router, Proxy Integration & Bandit Improvements

## Summary

Implemented Phase 4 (Router & proxy integration) and Phase 5 (Bandit improvements) of the Reflexion Loop feature as defined in `docs/superpowers/plans/2026-09-20-reflexion-loop-implementation.md` and `docs/superpowers/specs/2026-09-19-reflexion-loop-local-design.md` §4, §10.4, §10.9.

## Changes Made

### 1. Bandit Improvements (`crates/firstpass-proxy/src/bandit.rs`)
- Added `pub subagent_name: Option<String>` to `ContextBucket` (preserving standard derives: `Debug`, `Clone`, `Hash`, `Eq`, `PartialEq`).
- Updated `ContextBucket::from_features` to populate `subagent_name: f.subagent_name.clone()`.
- Implemented `pub fn observe_with_cycles(&mut self, ctx: &ContextBucket, rung: u32, verdict: Verdict, reflexion_cycles: u32)` applying the discount factor `1.0 / (1.0 + reflexion_cycles as f64)` to `Verdict::Pass` observations.
- Refactored `pub fn observe(&mut self, ctx: &ContextBucket, rung: u32, verdict: Verdict)` to delegate to `self.observe_with_cycles(ctx, rung, verdict, 0)` preserving backward compatibility.
- Added unit test `assisted_pass_is_discounted_vs_unassisted_pass` verifying 0 cycles vs 2 cycles discount behavior.
- Added unit test `context_bucket_distinguishes_subagent_names` verifying subagent bucket partition.

### 2. Reflexion Engine Seam (`crates/firstpass-proxy/src/reflexion.rs`)
- Updated `ReflexionCtx.gates` from `&'a [Box<dyn Gate + Send + Sync>]` to `&'a [Box<dyn Gate>]` to align directly with `EnforceCtx.gates` without trait object mismatch.

### 3. Router Integration (`crates/firstpass-proxy/src/router.rs`)
- Added `pub reflexion: Option<&'a firstpass_core::config::ReflexionConfig>` to `EnforceCtx`.
- Updated `run_ladder` signature to `async fn run_ladder(ctx: &EnforceCtx<'_>, mut pre_attempts: Vec<Attempt>) -> LadderRun` and prepended `pre_attempts` to `run.attempts`.
- Added reflexion pre-pass loop in `route_enforce`:
  - Builds `ReflexionCtx` from `ctx.reflexion` configuration, `ctx.ladder[0]` (or `start_rung`), `ctx.gates`, `ctx.providers`, and `ctx.prices`.
  - Runs `crate::reflexion::run_reflexion_loop(&rx_ctx).await`.
  - **Short-Circuit on Pass (`run.served_rung.is_some()`):** Skips ladder execution, constructs `Trace` with `final_.reflexion_cycles`, `final_.mentor_cost_usd`, `final_.reflexion_cycles_to_pass`, `final_.triggered_by_self_verify`, and `final_.reflexion_latency_capped`, calls `trace.recompute_savings()`, and returns immediately.
  - **Exhausted with `ReflexionExhaustedPolicy::Error`:** Emits `EngineOutcome::Failed("Reflexion loop exhausted".into())` with a failed trace record.
  - **Exhausted with `ReflexionExhaustedPolicy::ServeBestAttempt`:** Passes `run.attempts` as `pre_attempts` into `run_ladder`, and updates final trace cost/gate metrics and reflexion metadata.
- Added integration unit tests:
  - `reflexion_short_circuits_on_pass`: verifies 1 cycle pass short-circuits ladder and preserves trace audit receipts.
  - `reflexion_exhausted_error_policy_fails_immediately`: verifies error policy terminates cleanly without invoking ladder rungs.
  - `reflexion_exhausted_serve_best_falls_through_to_ladder`: verifies best-attempt fallback prepends reflexion attempts and climbs ladder to passing rung.

### 4. Proxy Pipeline Integration (`crates/firstpass-proxy/src/proxy.rs`)
- In `extract_features` and `extract_openai_features`, extracted `x-firstpass-subagent-name` header into `f.subagent_name`, falling back to `f.subagent`.
- In `shadow_enforce_route` and `handle_enforce`, passed `reflexion: route.reflexion.as_ref()` into `EnforceCtx`.
- Updated bandit observation in `handle_enforce` to call `b.observe_with_cycles(&bandit_ctx, attempt.rung, attempt.verdict, trace.final_.reflexion_cycles.unwrap_or(0))`.

## Verification Results

1. **Unit & Integration Tests:**
   - `cargo test --workspace` passed (448 unit tests + 12 integration tests across all workspace crates).
2. **Clippy:**
   - `cargo clippy --workspace --all-targets -- -D warnings` passed with 0 warnings/errors.
3. **Formatting:**
   - `cargo fmt --all --check` passed cleanly.
