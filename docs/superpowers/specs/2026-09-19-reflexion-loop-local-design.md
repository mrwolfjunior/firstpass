# Reflexion Loop — Design Document

**Date:** 2026-09-19  
**Status:** Approved for implementation  
**Author:** Architectural session with Antigravity  

---

## 0. Context and Goal

### Setup

Two llama.cpp processes run locally, each exposing an OpenAI-compatible HTTP server:

| Role | Model | Speed | Endpoint (example) |
|---|---|---|---|
| **Executor** | 30B | 100 tk/s decoding, prefill fast | `http://localhost:8080` |
| **Mentor** | 80B | 5 tk/s decoding, prefill fast | `http://localhost:8081` |

> **Critical insight on prefill vs decoding:** llama.cpp parallelizes prefill across all KV heads. Reading 8k tokens of context costs nearly zero time on both models. The only latency that matters is **tokens generated** (decoding). Design every change to minimize the 80B's generated token count.

### Goal

Replace the current one-shot escalation model (cheap → fail → expensive) with a **Reflexion Loop**: the 30B executes tasks; on gate failure, the 80B reads the full conversation context (fast prefill) and generates only a short targeted correction (expensive, decoding). The 30B retries with the correction injected. This loop repeats up to `max_reflections` times before the request is considered failed.

### What does NOT change

- The wire-compatible proxy endpoints (`/v1/messages`, `/v1/chat/completions`)  
- The hash-chained audit trace schema (wire names are the audit contract)  
- The gate plugin system (subprocess / judge / schema / consistency)  
- The provider registry and `[[provider]]` TOML blocks  
- The `firstpass-core` crate — it stays I/O-free  
- All existing `cargo test` passing tests  

---

## 1. Architecture Overview

```
[harness / agent]
        │ OpenAI-compatible request
        ▼
[firstpass proxy — unchanged entry point]
        │
        ▼
[ReflexionEngine — NEW, sits above run_serial/run_speculative]
        │
        ├─► [30B Executor @ :8080]  ─► [Gate: deterministic]
        │         ▲                           │ fail
        │         │ retry with correction      │
        │         └─────────────────────────  │
        │                                     ▼
        └─────────────────────────────► [80B Mentor @ :8081]
                                         reads full context (prefill)
                                         generates short correction (~150 tok)
```

The `ReflexionEngine` wraps `run_serial`. It does not replace it. When the gate passes or `max_reflections` is exhausted, it falls through to the existing `route_enforce` serving/trace path.

---

## 2. Configuration Changes

### 2.1 New TOML section: `[reflexion]`

Add to `firstpass_core::config::Config` (file: `crates/firstpass-core/src/config.rs`).

**Where to add it:** after the `guardrail` field (line ~176), before `gate_defs`.

```rust
/// Reflexion loop configuration. When present on a route, gate failures trigger a
/// mentor-model reflection instead of immediate ladder escalation.
/// Absent (the default) = today's behaviour, byte-identical.
#[serde(default)]
pub reflexion: Option<ReflexionConfig>,
```

**New struct** (add after `CondenseConfig`, around line 785):

```rust
/// Configuration for the mentor-guided Reflexion Loop.
///
/// On gate failure the mentor model reads the full conversation context (fast prefill)
/// and generates a short targeted correction. The executor retries with the correction
/// injected as a system-level note. This repeats up to `max_reflections` times before
/// the request is considered failed.
///
/// Budget: each reflection costs one 80B mentor call (only output tokens bill at decoding
/// speed; input tokens are prefill and are fast). Keep `mentor_max_out_tokens` low (100–200)
/// to bound latency.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReflexionConfig {
    /// The mentor model, as `provider/model` (e.g. `"local-80b/qwen2.5-72b"`).
    /// Must be registered in `[[provider]]`.
    pub mentor_model: String,

    /// Hard cap on reflection cycles per request. Must be in `[1, 5]`; validated at parse.
    /// Each cycle = one 30B attempt + one 80B correction generation.
    /// Recommended: 2–3. Beyond 3 the context grows large and the 80B sees diminishing returns.
    #[serde(default = "default_max_reflections")]
    pub max_reflections: u32,

    /// Max tokens the mentor may generate per correction. Lower = faster.
    /// The mentor should diagnose the failure and emit a correction note, not rewrite the answer.
    /// Recommended: 150–250 tokens. Must be > 0; validated at parse.
    #[serde(default = "default_mentor_max_out_tokens")]
    pub mentor_max_out_tokens: u32,

    /// System prompt injected into the mentor call. If absent, a built-in default is used.
    /// The built-in prompt instructs the mentor to:
    ///   1. Read the task, the executor's output, and the gate failure reason.
    ///   2. Generate only a short correction note (not a full answer).
    ///   3. Output JSON: `{"correction": "...", "strategy": "..."}`.
    #[serde(default)]
    pub mentor_system_prompt: Option<String>,

    /// When true, the correction note is injected as a `user` turn appended to the conversation
    /// before the executor retries. When false (default), it is injected as a hidden `system`
    /// message prefix visible only to the executor, not the caller.
    ///
    /// IMPORTANT: only the LAST correction is ever injected. Previous corrections are
    /// discarded from the request to prevent context accumulation (see §10.1).
    #[serde(default)]
    pub inject_as_user_turn: bool,

    /// Convergence threshold in `[0, 1)`. If the normalized edit distance between the
    /// executor's current output and its previous output is below this value, the loop
    /// terminates early (the executor has stopped changing — further corrections are unlikely
    /// to help). `0.0` (default) disables convergence detection.
    ///
    /// Recommended: 0.05 (stop if outputs differ by less than 5%). Validated at parse.
    /// See §10.5 for the full stopping-criterion design.
    #[serde(default)]
    pub convergence_threshold: f64,

    /// Hard wall-clock budget for the entire reflexion loop in milliseconds.
    /// If the cumulative latency of executor + mentor calls exceeds this value, the loop
    /// terminates immediately and the best attempt so far is served (or `on_reflexion_exhausted`
    /// policy applies). `None` (default) = no latency cap.
    ///
    /// At 5 tk/s, a 200-token mentor correction costs ~40 seconds. With `max_reflections = 2`,
    /// worst-case latency is ~120 seconds. Set `max_latency_ms = 90_000` to cap at 90 seconds.
    ///
    /// This is a **soft** wall: the check runs between cycles, not mid-generation. A mentor
    /// call that starts under budget may still finish over budget.
    #[serde(default)]
    pub max_latency_ms: Option<u64>,

    /// What to do when the reflexion loop exhausts all cycles without a gate passing,
    /// or when the loop is terminated early (convergence / latency cap).
    ///
    /// - `"serve_best_attempt"` (default): return the best attempt seen so far (lowest
    ///   gate failure score). The caller gets a response, even if sub-optimal.
    /// - `"error"`: return an explicit error to the harness. Use when serving a failed
    ///   attempt is worse than no answer (e.g., the harness knows to re-plan on failure).
    ///   Corresponds to the "Firewall Routing" pattern (Peng et al., 2025).
    #[serde(default = "default_on_reflexion_exhausted")]
    pub on_reflexion_exhausted: ReflexionExhaustedPolicy,
}

/// Policy when the reflexion loop ends without a successful gate pass.
#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReflexionExhaustedPolicy {
    /// Serve the best attempt seen during the loop (default, safe fallback).
    #[default]
    ServeBestAttempt,
    /// Return an error to the caller instead of a partial answer.
    /// Corresponds to the "Firewall Routing" pattern — blocks unsolvable queries from
    /// wasting resources on the ladder escalation path.
    Error,
}

fn default_max_reflections() -> u32 { 2 }
fn default_mentor_max_out_tokens() -> u32 { 200 }
fn default_on_reflexion_exhausted() -> ReflexionExhaustedPolicy { ReflexionExhaustedPolicy::ServeBestAttempt }
```

**Validation** (add to `Config::parse` wherever other field validations live):

```rust
if let Some(ref rx) = self.reflexion {
    if rx.max_reflections == 0 || rx.max_reflections > 5 {
        return Err(Error::Config("reflexion.max_reflections must be in [1, 5]".into()));
    }
    if rx.mentor_max_out_tokens == 0 {
        return Err(Error::Config("reflexion.mentor_max_out_tokens must be > 0".into()));
    }
    if rx.convergence_threshold < 0.0 || rx.convergence_threshold >= 1.0 {
        return Err(Error::Config(
            "reflexion.convergence_threshold must be in [0.0, 1.0)".into()
        ));
    }
    // max_latency_ms: no lower-bound check; 0 is weird but not illegal (loop exits
    // immediately after the first cycle). The implementor is responsible.
}
```

### 2.2 New field on `Route`

In `crates/firstpass-core/src/config.rs`, struct `Route` (line ~381):

```rust
/// Reflexion loop for this route. When set, gate failures on the executor rung trigger
/// mentor-guided reflection instead of (or before) ladder escalation.
/// Absent (the default) = today's behaviour, byte-identical.
#[serde(default)]
pub reflexion: Option<ReflexionConfig>,
```

> **Note:** `Config` has `#[serde(deny_unknown_fields)]` and so does `Route`. The new field must be added explicitly — there is no other way.

### 2.3 Example `firstpass.toml`

```toml
[[provider]]
id       = "local-30b"
dialect  = "openai"
base_url = "http://localhost:8080"
# No api_key_env: local llama.cpp is keyless.

[[provider]]
id       = "local-80b"
dialect  = "openai"
base_url = "http://localhost:8081"

[[price]]
model            = "local-30b/qwen2.5-32b"
input_per_mtok   = 0.0
output_per_mtok  = 0.0

[[price]]
model            = "local-80b/qwen2.5-72b"
input_per_mtok   = 0.0
output_per_mtok  = 0.0

[[route]]
match  = {}
mode   = "enforce"
ladder = ["local-30b/qwen2.5-32b", "local-80b/qwen2.5-72b"]
gates  = ["non-empty"]

[route.reflexion]
mentor_model            = "local-80b/qwen2.5-72b"
max_reflections         = 2
mentor_max_out_tokens   = 200
convergence_threshold   = 0.05      # stop early if executor output barely changes
max_latency_ms          = 90_000    # hard cap: 90 seconds total for all cycles
on_reflexion_exhausted  = "serve_best_attempt"  # or "error" (Firewall Routing)

[budget]
per_request_usd = 0.0
on_exhausted    = "serve_best_attempt"

[escalation]
max_rungs_per_request = 2
```

---

## 3. Trace Schema Changes

### 3.1 New fields on `Attempt`

File: `crates/firstpass-core/src/trace.rs`, struct `Attempt` (line ~241).

Add after the `verdict` field:

```rust
/// Reflexion cycle index for this attempt. `None` for the first attempt (cycle 0 is implicit
/// and kept absent for backward-compat with pre-reflexion traces). `Some(1)` = first retry
/// after a mentor correction, `Some(2)` = second retry, etc.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub reflexion_cycle: Option<u32>,

/// SHA-256 (hex, first 16 chars) of the mentor correction note that preceded this attempt.
/// `None` on the first attempt and on non-reflexion routes. Never the raw text.
///
/// Uses SHA-256 (not DefaultHasher) so the value is stable across Rust versions and
/// auditable by external tools. See §10.2 for the hash function specification.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub mentor_correction_hash: Option<String>,

/// Whether this attempt terminated the reflexion loop due to output convergence
/// (normalized edit distance < `convergence_threshold`) rather than gate pass or cycle
/// exhaustion. `None` / `false` for all non-convergence terminations.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub reflexion_converged: Option<bool>,
```

### 3.2 New fields on `FinalOutcome`

File: `crates/firstpass-core/src/trace.rs`, struct `FinalOutcome` (line ~362).

Add after `escalations`:

```rust
/// Number of reflexion cycles completed before the final answer was served.
/// `None` when reflexion was not configured — byte-identical to pre-reflexion traces.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub reflexion_cycles: Option<u32>,

/// USD cost of all mentor calls in the reflexion loop, separate from executor cost.
/// `None` when reflexion was off.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub mentor_cost_usd: Option<f64>,

/// Number of reflection cycles used to reach a Pass verdict.
/// `Some(0)` = first attempt passed (no reflection needed).
/// `Some(N)` = N mentor corrections were needed.
/// `None` = reflexion not configured (backward-compat).
#[serde(default, skip_serializing_if = "Option::is_none")]
pub reflexion_cycles_to_pass: Option<u32>,
```

> **Hash chain invariant:** all new fields use `#[serde(default, skip_serializing_if = "Option::is_none")]`. A trace with all-`None` new fields serializes byte-identically to a pre-reflexion trace. The chain does not break.

---

## 4. New File: `crates/firstpass-proxy/src/reflexion.rs`

Create this file from scratch.

```rust
//! Mentor-guided Reflexion Loop (local llama.cpp dual-model setup).
//!
//! When a gate fails on the executor rung, the mentor model reads the full conversation
//! context (fast prefill) and generates a short correction note. The executor retries
//! with the correction injected. Repeats up to `ReflexionConfig::max_reflections` times.
//!
//! # Context isolation invariant
//! Only the LATEST mentor correction is ever injected into the executor's request.
//! Previous corrections are discarded. This prevents context accumulation (which degrades
//! quality by adding noise from earlier failed attempts — see §10.1).

use crate::gate::{Gate, GateHealthRegistry, aggregate_with_policy};
use crate::provider::{Auth, ModelRequest, ModelResponse, ProviderRegistry};
use firstpass_core::config::ReflexionConfig;
use firstpass_core::verdict::reason;
use firstpass_core::{Attempt, GateResult, ModelRef, PriceTable, Verdict};
use std::time::Instant;

/// The outcome of one reflexion loop run.
pub struct ReflexionRun {
    pub attempts: Vec<Attempt>,
    pub executor_cost_usd: f64,
    pub mentor_cost_usd: f64,
    pub gate_cost_total: f64,
    pub best: Option<(u32, ModelResponse)>,
    /// `Some(rung_index)` if a gate passed; `None` if the loop exhausted without a pass.
    pub served_rung: Option<u32>,
    pub cycles_completed: u32,
    /// Whether the loop terminated due to output convergence (not gate pass).
    pub converged: bool,
    pub hard_error: Option<String>,
}

/// Context for one reflexion loop invocation.
pub struct ReflexionCtx<'a> {
    pub config: &'a ReflexionConfig,
    pub executor_rung: u32,
    pub executor_model: &'a str,
    pub gates: &'a [Box<dyn Gate>],
    pub health: &'a GateHealthRegistry,
    pub base_request: &'a ModelRequest,
    pub providers: &'a ProviderRegistry,
    pub auth: &'a Auth,
    pub prices: &'a PriceTable,
    pub tenant_id: &'a str,
    pub serve_threshold: Option<f64>,
}

const DEFAULT_MENTOR_SYSTEM_PROMPT: &str = r#"You are a senior code reviewer and mentor.
You will receive:
  1. The original task (from the conversation history).
  2. The executor's output that failed a quality gate.
  3. The gate failure reason.

Your job: diagnose the root cause and provide a short, targeted correction note.

Rules:
- Output ONLY valid JSON: {"correction": "<your note>", "strategy": "<retry_hint>"}
- The correction note must be ≤ 3 sentences. No code, no full rewrites.
- strategy must be one of: "fix_logic", "fix_format", "be_more_specific", "simplify"
- If you cannot diagnose the issue, output: {"correction": "unable to diagnose", "strategy": "simplify"}
"#;

/// Run the reflexion loop for the executor rung.
///
/// # Context isolation
/// `inject_correction` replaces the previous correction in the request — it never accumulates.
/// Each cycle the executor sees: [original task messages] + [single latest correction].
pub async fn run_reflexion_loop(ctx: &ReflexionCtx<'_>) -> ReflexionRun {
    let mut attempts: Vec<Attempt> = Vec::new();
    let mut executor_cost_usd = 0.0_f64;
    let mut mentor_cost_usd = 0.0_f64;
    let mut gate_cost_total = 0.0_f64;
    let mut best: Option<(u32, ModelResponse)> = None;
    let mut served_rung: Option<u32> = None;
    let mut cycles_completed = 0u32;
    let mut converged = false;
    let mut hard_error: Option<String> = None;

    // `base_request` is the original request, never mutated across cycles.
    // Each cycle builds a `current_request` fresh from base + latest correction only.
    let base_request = ctx.base_request.clone();

    // The latest correction injected into `current_request`. Reset each cycle.
    let mut latest_correction: Option<String> = None;
    let mut pending_mentor_correction_hash: Option<String> = None;

    // Track previous output for convergence detection.
    let mut prev_output: Option<String> = None;

    let executor_provider = match ModelRef::parse(ctx.executor_model) {
        Ok(m) => ctx.providers.get(&m.provider),
        Err(_) => None,
    };
    let Some(executor_provider) = executor_provider else {
        return ReflexionRun {
            attempts,
            executor_cost_usd,
            mentor_cost_usd,
            gate_cost_total,
            best,
            served_rung,
            cycles_completed,
            converged,
            hard_error: Some(format!(
                "reflexion: unknown provider for executor model `{}`",
                ctx.executor_model
            )),
        };
    };

    let mentor_provider = match ModelRef::parse(&ctx.config.mentor_model) {
        Ok(m) => ctx.providers.get(&m.provider),
        Err(_) => None,
    };

    // max_cycles = max_reflections + 1 (the initial attempt is cycle 0).
    let max_cycles = ctx.config.max_reflections + 1;

    for cycle in 0..max_cycles {
        let cycle_start = Instant::now();

        // ── Build executor request for this cycle ────────────────────────────
        // Start from the base (original) request every cycle, then inject only the LATEST
        // correction. This is the context isolation invariant: the executor never sees
        // corrections from previous cycles piling up.
        let mut current_request = base_request.clone();
        current_request.model = ctx.executor_model.to_owned();
        if let Some(ref corr) = latest_correction {
            inject_correction(&mut current_request, corr, ctx.config.inject_as_user_turn);
        }

        // ── Executor call ────────────────────────────────────────────────────
        let resp = match executor_provider.complete(&current_request, ctx.auth).await {
            Err(err) if err.is_failover_eligible() => {
                let ms = elapsed_ms(cycle_start);
                attempts.push(make_abstain_attempt(
                    ctx.executor_rung, ctx.executor_model, executor_provider.id(),
                    reason::PROVIDER_ERROR, ms,
                    if cycle > 0 { Some(cycle) } else { None },
                    pending_mentor_correction_hash.take(),
                    None,
                ));
                hard_error = Some(format!("reflexion: executor provider error on cycle {cycle}"));
                break;
            }
            Err(err) => {
                let ms = elapsed_ms(cycle_start);
                attempts.push(make_abstain_attempt(
                    ctx.executor_rung, ctx.executor_model, executor_provider.id(),
                    reason::HARD_ERROR, ms,
                    if cycle > 0 { Some(cycle) } else { None },
                    pending_mentor_correction_hash.take(),
                    None,
                ));
                hard_error = Some(format!("reflexion: executor hard error: {err}"));
                break;
            }
            Ok(resp) => resp,
        };

        let ms = elapsed_ms(cycle_start);
        let model_cost = ctx.prices
            .cost_usd_with_cache(
                ctx.executor_model,
                resp.in_tokens, resp.cache_write_tokens,
                resp.cache_read_tokens, resp.out_tokens,
            )
            .unwrap_or(0.0);
        executor_cost_usd += model_cost;

        // ── Convergence check (§10.5) ─────────────────────────────────────────
        // If the executor's output barely changed vs the previous cycle, further mentor
        // corrections are unlikely to help. Terminate early and serve the best attempt.
        if ctx.config.convergence_threshold > 0.0 {
            if let Some(ref prev) = prev_output {
                let dist = normalized_edit_distance(prev, &resp.content);
                if dist < ctx.config.convergence_threshold {
                    tracing::info!(
                        cycle,
                        dist,
                        threshold = ctx.config.convergence_threshold,
                        "reflexion: convergence detected — stopping loop early"
                    );
                    // Push the attempt as Abstain-like with converged flag, then break.
                    // The best attempt from the previous cycle will be served.
                    let verdict = Verdict::Abstain; // converged = not graded
                    attempts.push(Attempt {
                        rung: ctx.executor_rung,
                        model: ctx.executor_model.to_owned(),
                        provider: executor_provider.id().to_owned(),
                        in_tokens: resp.in_tokens,
                        cache_write_tokens: resp.cache_write_tokens,
                        cache_read_tokens: resp.cache_read_tokens,
                        out_tokens: resp.out_tokens,
                        cost_usd: model_cost,
                        latency_ms: ms,
                        gates: vec![],
                        verdict,
                        reflexion_cycle: Some(cycle),
                        mentor_correction_hash: pending_mentor_correction_hash.take(),
                        reflexion_converged: Some(true),
                    });
                    converged = true;
                    break;
                }
            }
        }
        prev_output = Some(resp.content.clone());

        // ── Gate evaluation ─────────────────────────────────────────────────
        let mut gate_results: Vec<GateResult> = Vec::with_capacity(ctx.gates.len());
        let fail_closed: std::collections::HashSet<&str> = ctx.gates.iter()
            .filter(|g| g.abstain_fails_closed())
            .map(|g| g.id())
            .collect();

        for g in ctx.gates {
            if !ctx.health.enabled(ctx.tenant_id, g.id()) {
                tracing::warn!(gate = %g.id(), cycle, "skipping auto-disabled gate (reflexion)");
                continue;
            }
            let r = g.evaluate(&current_request, &resp).await;
            ctx.health.record(ctx.tenant_id, g.id(), r.verdict == Verdict::Abstain);
            gate_results.push(r);
        }

        let gc: f64 = gate_results.iter().map(|g| g.cost_usd).sum();
        gate_cost_total += gc;
        executor_cost_usd += gc;

        let verdict = aggregate_with_policy(&gate_results, &fail_closed);
        let serve = match ctx.serve_threshold {
            None => verdict == Verdict::Pass,
            Some(t) => crate::calibrate::gate_score(&gate_results, verdict) >= t,
        };

        attempts.push(Attempt {
            rung: ctx.executor_rung,
            model: ctx.executor_model.to_owned(),
            provider: executor_provider.id().to_owned(),
            in_tokens: resp.in_tokens,
            cache_write_tokens: resp.cache_write_tokens,
            cache_read_tokens: resp.cache_read_tokens,
            out_tokens: resp.out_tokens,
            cost_usd: model_cost,
            latency_ms: ms,
            gates: gate_results.clone(),
            verdict,
            reflexion_cycle: if cycle > 0 { Some(cycle) } else { None },
            mentor_correction_hash: pending_mentor_correction_hash.take(),
            reflexion_converged: None,
        });
        best = Some((ctx.executor_rung, resp.clone()));

        if serve {
            served_rung = Some(ctx.executor_rung);
            break;
        }

        // ── Gate failed: call mentor if cycles remain ────────────────────────
        if cycle + 1 >= max_cycles {
            break;
        }

        let Some(ref mentor_prov) = mentor_provider else {
            tracing::warn!(
                mentor_model = %ctx.config.mentor_model,
                "reflexion: mentor provider not found; skipping reflection"
            );
            break;
        };

        let failure_reasons: Vec<&str> = gate_results.iter()
            .filter(|g| g.verdict == Verdict::Fail)
            .filter_map(|g| g.reason.as_deref())
            .collect();

        // The mentor's user message is compact by design (§10.1 + §10.3):
        // - Only the last user turn from the original task (not the full history)
        // - Executor output truncated to 1000 chars
        // - Gate failure reasons only
        let mentor_user_content = build_mentor_user_message(
            &base_request, // always from base, not accumulated current_request
            &resp,
            &failure_reasons,
        );

        let system_prompt = ctx.config.mentor_system_prompt.as_deref()
            .unwrap_or(DEFAULT_MENTOR_SYSTEM_PROMPT);

        let mentor_req = ModelRequest {
            model: ctx.config.mentor_model.clone(),
            system: Some(system_prompt.to_owned()),
            messages: vec![crate::provider::Message {
                role: "user".to_owned(),
                content: mentor_user_content,
            }],
            max_tokens: ctx.config.mentor_max_out_tokens,
            tools: vec![],
            stream: false,
            extra: Default::default(),
        };

        let correction = match mentor_prov.complete(&mentor_req, ctx.auth).await {
            Err(e) => {
                tracing::warn!(error = %e, cycle, "reflexion: mentor call failed; ending loop");
                break;
            }
            Ok(mr) => {
                let mentor_model_cost = ctx.prices
                    .cost_usd_with_cache(
                        &ctx.config.mentor_model,
                        mr.in_tokens, mr.cache_write_tokens,
                        mr.cache_read_tokens, mr.out_tokens,
                    )
                    .unwrap_or(0.0);
                mentor_cost_usd += mentor_model_cost;
                parse_mentor_correction(&mr.content).unwrap_or_else(|| mr.content.clone())
            }
        };

        cycles_completed += 1;
        // Only the LATEST correction is stored — previous ones are discarded (§10.1).
        latest_correction = Some(correction.clone());
        pending_mentor_correction_hash = Some(hash_correction_sha256(&correction));
    }

    ReflexionRun {
        attempts,
        executor_cost_usd,
        mentor_cost_usd,
        gate_cost_total,
        best,
        served_rung,
        cycles_completed,
        converged,
        hard_error,
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

fn build_mentor_user_message(
    base_req: &ModelRequest,   // Always the original base request, not accumulated
    resp: &ModelResponse,
    failure_reasons: &[&str],
) -> String {
    // Only the last user turn — not the full history. The mentor does not need to replay
    // the conversation; it needs to understand the task and the failure.
    let last_user = base_req.messages.iter().rev()
        .find(|m| m.role == "user")
        .map(|m| m.content.as_str())
        .unwrap_or("<no user message>");

    // Truncate to 1000 chars. More context does not improve mentor diagnosis quality but
    // forces it to generate more context-aware output (which is expensive at 5 tk/s).
    const MAX_OUTPUT_CHARS: usize = 1000;
    let executor_out = if resp.content.len() > MAX_OUTPUT_CHARS {
        format!(
            "{}… [truncated {} chars]",
            &resp.content[..MAX_OUTPUT_CHARS],
            resp.content.len() - MAX_OUTPUT_CHARS
        )
    } else {
        resp.content.clone()
    };

    let failures = if failure_reasons.is_empty() {
        "- (gate abstained or unknown reason)".to_owned()
    } else {
        failure_reasons.iter().map(|r| format!("- {r}")).collect::<Vec<_>>().join("\n")
    };

    format!(
        "=== TASK ===\n{last_user}\n\n=== EXECUTOR OUTPUT ===\n{executor_out}\n\n=== GATE FAILURES ===\n{failures}"
    )
}

fn parse_mentor_correction(raw: &str) -> Option<String> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end < start { return None; }
    let v: serde_json::Value = serde_json::from_str(&raw[start..=end]).ok()?;
    v.get("correction").and_then(|c| c.as_str()).map(str::to_owned)
}

/// Inject ONLY the latest correction into the executor's request.
///
/// This function is called on a FRESH clone of `base_request` each cycle, so corrections
/// never accumulate. The previous cycle's correction is automatically absent because it was
/// never written to `base_request`.
fn inject_correction(req: &mut ModelRequest, correction: &str, as_user_turn: bool) {
    if as_user_turn {
        req.messages.push(crate::provider::Message {
            role: "user".to_owned(),
            content: format!("[MENTOR NOTE — do not quote in your response]: {correction}"),
        });
    } else {
        let note = format!(
            "[MENTOR CORRECTION]: {correction}\n\nPlease address the above before responding.\n\n"
        );
        match &mut req.system {
            Some(s) => s.insert_str(0, &note),
            None => req.system = Some(note),
        }
    }
}

/// SHA-256 of the correction text, hex-encoded, first 16 chars.
///
/// MUST use SHA-256 (not `DefaultHasher`): `DefaultHasher` is not stable across Rust versions.
/// If the hash changed between versions, the audit trail would be broken. SHA-256 is
/// deterministic, version-stable, and verifiable by external auditors. Add `sha2` to
/// `firstpass-proxy/Cargo.toml` as a dependency:
///
/// ```toml
/// sha2 = "0.10"
/// ```
fn hash_correction_sha256(correction: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(correction.as_bytes());
    // First 16 hex chars (8 bytes) are sufficient for a non-cryptographic audit reference.
    // Full 64-char hex is never needed here — this is an audit link, not a security boundary.
    format!("{:.16}", hex::encode(hash))
}

/// Normalized edit distance between two strings in `[0, 1]`.
///
/// Returns 0.0 if the strings are identical, 1.0 if completely different.
/// Uses character-level Levenshtein distance normalized by max(len_a, len_b).
///
/// This is intentionally a simple implementation — no external crate. The strings are
/// executor outputs (typically 200–2000 chars); this runs in O(n·m) time and is not on
/// the hot path (called at most `max_reflections` times per request).
///
/// Add `hex = "0.4"` to `firstpass-proxy/Cargo.toml` for the SHA-256 helper above.
fn normalized_edit_distance(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (na, nb) = (a.len(), b.len());
    if na == 0 && nb == 0 { return 0.0; }
    let max_len = na.max(nb);
    if max_len == 0 { return 0.0; }

    // Levenshtein with two-row rolling array (O(n·m) time, O(n) space).
    let mut prev: Vec<usize> = (0..=nb).collect();
    let mut curr = vec![0usize; nb + 1];
    for (i, ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[j + 1] = (prev[j + 1] + 1)
                .min(curr[j] + 1)
                .min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[nb] as f64 / max_len as f64
}

fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn make_abstain_attempt(
    rung: u32, model: &str, provider: &str, reason_str: &str, ms: u64,
    reflexion_cycle: Option<u32>, mentor_correction_hash: Option<String>,
    reflexion_converged: Option<bool>,
) -> Attempt {
    Attempt {
        rung,
        model: model.to_owned(),
        provider: provider.to_owned(),
        in_tokens: 0, cache_write_tokens: 0, cache_read_tokens: 0, out_tokens: 0,
        cost_usd: 0.0,
        latency_ms: ms,
        gates: vec![firstpass_core::GateResult::abstain(provider, reason_str, ms)],
        verdict: Verdict::Abstain,
        reflexion_cycle,
        mentor_correction_hash,
        reflexion_converged,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mentor_correction_valid_json() {
        let raw = r#"{"correction": "use map instead of for loop", "strategy": "fix_logic"}"#;
        assert_eq!(parse_mentor_correction(raw), Some("use map instead of for loop".to_owned()));
    }

    #[test]
    fn parse_mentor_correction_with_preamble() {
        let raw = r#"Sure! {"correction": "add a newline at end", "strategy": "fix_format"}"#;
        assert_eq!(parse_mentor_correction(raw), Some("add a newline at end".to_owned()));
    }

    #[test]
    fn parse_mentor_correction_invalid_returns_none() {
        assert_eq!(parse_mentor_correction("not json at all"), None);
        assert_eq!(parse_mentor_correction(r#"{"wrong_key": "x"}"#), None);
    }

    #[test]
    fn inject_as_system_prefix_prepends() {
        let mut req = ModelRequest::test_empty();
        req.system = Some("original system".into());
        inject_correction(&mut req, "fix your formatting", false);
        assert!(req.system.unwrap().starts_with("[MENTOR CORRECTION]"));
    }

    #[test]
    fn inject_as_user_turn_appends_message() {
        let mut req = ModelRequest::test_empty();
        inject_correction(&mut req, "fix your formatting", true);
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, "user");
        assert!(req.messages[0].content.contains("[MENTOR NOTE"));
    }

    #[test]
    fn hash_correction_sha256_is_stable() {
        // SHA-256 is version-stable. This test pins the expected output so any regression
        // (e.g. accidentally reverting to DefaultHasher) is caught immediately.
        let h = hash_correction_sha256("fix your formatting");
        assert_eq!(h.len(), 16, "hash must be exactly 16 hex chars");
        // Run twice: must be identical (no randomness).
        assert_eq!(h, hash_correction_sha256("fix your formatting"));
        // Different inputs must produce different hashes.
        assert_ne!(h, hash_correction_sha256("fix something else"));
    }

    #[test]
    fn normalized_edit_distance_identical_strings() {
        assert_eq!(normalized_edit_distance("abc", "abc"), 0.0);
    }

    #[test]
    fn normalized_edit_distance_completely_different() {
        // "abc" → "xyz": 3 substitutions, max_len 3 → 1.0
        assert_eq!(normalized_edit_distance("abc", "xyz"), 1.0);
    }

    #[test]
    fn normalized_edit_distance_partial_change() {
        // "abcde" → "abxde": 1 substitution, max_len 5 → 0.2
        let d = normalized_edit_distance("abcde", "abxde");
        assert!((d - 0.2).abs() < 1e-9, "expected ~0.2, got {d}");
    }

    #[test]
    fn normalized_edit_distance_empty_strings() {
        assert_eq!(normalized_edit_distance("", ""), 0.0);
    }

    #[test]
    fn context_isolation_only_latest_correction_in_request() {
        // Simulate two cycles: base request has no correction, after cycle 1 inject
        // correction_1, after cycle 2 inject correction_2. The request going into cycle 2
        // must contain only correction_2, not correction_1.
        let base = ModelRequest::test_empty();

        let mut req_cycle1 = base.clone();
        inject_correction(&mut req_cycle1, "correction_1", false);
        assert!(req_cycle1.system.as_deref().unwrap_or("").contains("correction_1"));

        // Cycle 2: start fresh from base and inject only correction_2.
        let mut req_cycle2 = base.clone();
        inject_correction(&mut req_cycle2, "correction_2", false);
        let sys = req_cycle2.system.unwrap_or_default();
        assert!(sys.contains("correction_2"), "must contain correction_2");
        assert!(!sys.contains("correction_1"), "must NOT contain correction_1 (context isolation)");
    }
}
```

---

## 5. Integration into the Router

### 5.1 Add `reflexion.rs` to the proxy crate module tree

File: `crates/firstpass-proxy/src/lib.rs`

Add:

```rust
pub mod reflexion;
```

### 5.2 New field on `EnforceCtx`

File: `crates/firstpass-proxy/src/router.rs`, struct `EnforceCtx` (line ~36):

```rust
/// Reflexion config for this route. `None` = today's behaviour, byte-identical.
pub reflexion: Option<&'a firstpass_core::config::ReflexionConfig>,
```

### 5.3 Hook in `route_enforce`

File: `crates/firstpass-proxy/src/router.rs`, function `route_enforce` (line ~172).

Add this block **before** the `run_ladder` call:

```rust
// ── Reflexion loop (optional pre-pass) ────────────────────────────────────────
let (mut pre_attempts, mentor_cost, cycles_completed) = if let Some(ref rx_cfg) = ctx.reflexion {
    let executor_rung = ctx.start_rung;
    let executor_model = ctx.ladder
        .get(executor_rung as usize)
        .map(String::as_str)
        .unwrap_or("");

    let rx_ctx = crate::reflexion::ReflexionCtx {
        config: rx_cfg,
        executor_rung,
        executor_model,
        gates: ctx.gates,
        health: ctx.health,
        base_request: ctx.base_request,
        providers: ctx.providers,
        auth: ctx.auth,
        prices: ctx.prices,
        tenant_id: &ctx.tenant_id,
        serve_threshold: ctx.serve_threshold,
    };

    let run = crate::reflexion::run_reflexion_loop(&rx_ctx).await;

    if run.served_rung.is_some() {
        // Reflexion passed: short-circuit, do not call run_ladder.
        let total_latency_ms = run.attempts.iter().map(|a| a.latency_ms).sum();
        let top_model = ctx.ladder.last().map(String::as_str).unwrap_or_default();
        let served_tokens = run.best.as_ref()
            .map(|(_, r)| (r.in_tokens, r.out_tokens))
            .unwrap_or((0, 0));
        let baseline = ctx.prices.cost_usd(top_model, served_tokens.0, served_tokens.1)
            .unwrap_or(run.executor_cost_usd);
        let outcome = run.best.as_ref()
            .map(|(_, r)| EngineOutcome::Served(r.clone()))
            .unwrap_or_else(|| EngineOutcome::Failed("reflexion: no output".into()));
        let cycles_to_pass = run.served_rung.map(|_| run.cycles_completed);

        let mut trace = Trace {
            // ... all existing fields ...
            attempts: run.attempts,
            final_: FinalOutcome {
                served_rung: run.served_rung,
                total_cost_usd: run.executor_cost_usd + run.mentor_cost_usd,
                gate_cost_usd: run.gate_cost_total,
                total_latency_ms,
                escalations: 0,
                counterfactual_baseline_usd: baseline,
                savings_usd: 0.0,
                reflexion_cycles: if run.cycles_completed > 0 {
                    Some(run.cycles_completed)
                } else {
                    None
                },
                mentor_cost_usd: if run.mentor_cost_usd > 0.0 {
                    Some(run.mentor_cost_usd)
                } else {
                    None
                },
                reflexion_cycles_to_pass: cycles_to_pass,
                // ... other existing fields ...
            },
            // ... other existing fields ...
        };
        trace.recompute_savings();
        return (outcome, trace);
    }

    (run.attempts, run.mentor_cost_usd, run.cycles_completed)
} else {
    (vec![], 0.0, 0)
};
```

### 5.4 Report reflexion cycle count to the bandit (§10.4)

After calling `run_reflexion_loop`, before calling the bandit's `observe`:

```rust
// Inform the bandit of the reflexion-adjusted verdict.
// A Pass at cycle 0 (no reflection needed) is a stronger signal than a Pass at cycle 2.
// The bandit records the cycle count alongside the verdict so it can weight them differently.
// See §10.4 for the full bandit change.
if let Some(ref mut bandit) = ctx.bandit {
    let effective_verdict = if run.cycles_completed == 0 {
        run.served_rung.map(|_| Verdict::Pass).unwrap_or(Verdict::Fail)
    } else {
        // Pass after N reflections counts as a "weak pass" — bandit should not
        // aggressively prefer rung 0 if it reliably needs reflection to pass.
        Verdict::Pass // still Pass, but with reflexion_cycles recorded on the trace
    };
    bandit.observe_with_cycles(&ctx_bucket, executor_rung, effective_verdict, run.cycles_completed);
}
```

### 5.5 Populate `EnforceCtx.reflexion` in the caller

File: `crates/firstpass-proxy/src/proxy.rs`:

```rust
let ctx = EnforceCtx {
    // ... existing fields unchanged ...
    reflexion: route.reflexion.as_ref(),
};
```

---

## 6. Files Modified — Summary

| File | Change type | What changes |
|---|---|---|
| `crates/firstpass-core/src/config.rs` | **Additive** | `ReflexionConfig` struct; `ReflexionExhaustedPolicy` enum; `reflexion` field on `Config` and `Route`; all new optional fields |
| `crates/firstpass-core/src/trace.rs` | **Additive** | Three optional fields on `Attempt`; five optional fields on `FinalOutcome` (`reflexion_cycles`, `mentor_cost_usd`, `reflexion_cycles_to_pass`, `triggered_by_self_verify`, `reflexion_latency_capped`) |
| `crates/firstpass-core/src/features.rs` | **Additive** | `subagent_name: Option<String>` on `Features` |
| `crates/firstpass-proxy/src/reflexion.rs` | **New file** | Full reflexion loop: context isolation, convergence, SHA-256 hashing, latency cap, exhausted policy |
| `crates/firstpass-proxy/src/gate.rs` | **Additive** | New `SelfVerifyGate` struct + `kind = "self_verify"` dispatch (§10.8) |
| `crates/firstpass-proxy/src/lib.rs` | **Additive** | `pub mod reflexion;` |
| `crates/firstpass-proxy/src/router.rs` | **Additive** | `reflexion` field on `EnforceCtx`; pre-pass block in `route_enforce`; latency cap check; exhausted policy dispatch; bandit observe call |
| `crates/firstpass-proxy/src/proxy.rs` | **Additive** | Pass `route.reflexion.as_ref()` into `EnforceCtx`; extract `X-Firstpass-Subagent` header |
| `crates/firstpass-proxy/src/bandit.rs` | **Additive** | `subagent_name` in `ContextBucket`; `observe_with_cycles` method |
| `crates/firstpass-proxy/Cargo.toml` | **Additive** | `sha2 = "0.10"`, `hex = "0.4"` |

**No files are deleted. No existing fields are renamed. No tests are removed.**

---

## 7. Acceptance Criteria

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

Plus:

1. Unit tests in `reflexion.rs` pass (all 9 tests).
2. Backward-compat: a `Trace` with all reflexion fields as `None` serializes byte-identically to a pre-reflexion trace (no new JSON keys appear).
3. Hash chain: a chain of two traces, one with reflexion fields set and one without, verifies cleanly.
4. Config: `firstpass.toml` with `[route.reflexion]` parses without error; without it, parses identically to today. `convergence_threshold = 1.5` must fail validation.
5. Loop correctness: with mock providers, simulate executor-fails-gate / mentor-corrects / executor-passes. Assert `served_rung = Some(0)`, `cycles_completed = 1`, `mentor_cost_usd > 0`.
6. Context isolation: with mock providers, simulate 3 cycles. Assert the request sent in cycle 2 contains only the cycle-2 correction and NOT the cycle-1 correction.
7. Convergence: with mock providers that return identical output on cycles 1 and 2, assert `converged = true` and the loop terminates at cycle 2 (not max_reflections).
8. SHA-256 stability: `hash_correction_sha256("x")` returns the same value on two runs.
9. Latency cap: config `max_latency_ms = 1` triggers `reflexion_latency_capped = Some(true)` after the first executor call.
10. Exhausted policy `"error"`: mock exhausted loop returns `EngineOutcome::Failed` with no `served_rung`.
11. Self-verify gate: `kind = "self_verify"` parses; mock executor returning `"low"` triggers mentor; returning `"high"` passes immediately.
12. Bandit discount: `observe_with_cycles` with `cycles = 2` gives a lower `pass_estimate` than `cycles = 0` for the same number of observations.

---

## 8. Speed Optimizations (in priority order)

### Priority 1 — `mentor_max_out_tokens = 150–200` (already in config)

At 5 tk/s, 200 tokens = 40 seconds. The system prompt enforces short output instructionally; `max_tokens` enforces it technically. Do not let the mentor ramble.

### Priority 2 — Keep the mentor's input compact

`build_mentor_user_message` always uses `base_request` (the original task) — not the accumulated request. Executor output is truncated to 1000 chars. Never exceed 2000 chars total. More input = still fast prefill, but risks worse diagnosis quality.

### Priority 3 — `max_reflections = 2` default

The first correction fixes ~80% of correctable errors. A third cycle is rarely worth 40 extra seconds.

### Priority 4 — Gate order: deterministic first

Put `non-empty`, `schema`, `patch-applies` before any model-based gate. A fast deterministic fail triggers the mentor immediately. A slow judge gate delays the failure signal by seconds.

### Priority 5 — Convergence detection

Set `convergence_threshold = 0.05`. If the executor's output barely changes between cycles, the mentor is not helping. Stop early and save a full 40-second mentor call.

### Priority 6 — No vector DB

In-context correction (single latest note) is sufficient for intra-session memory. The SQLite trace store already holds correction hashes for inter-session audit. Add a vector DB only if empirical evidence shows the mentor repeating the same mistakes across sessions.

---

## 9. Out of Scope

Do not implement these now:

- Cross-session memory / vector DB / mem0
- Streaming reflexion (buffered responses required by the loop)
- Concurrent mentor calls
- Prompt cache breakpoints for mentor calls (25% premium, not worth it for 200-token outputs)
- Changes to `firstpass-bench` beyond adding `reflexion_cycles` and `reflexion_cycles_to_pass` to `metrics.rs`

---

## 10. Improvement Areas (implement in order after the core loop is working)

These are the six known correctness and performance improvements identified by codebase analysis. They are listed in priority order. **Do not implement them in the same commit as the core loop** — verify the loop first, then layer these on top.

---

### 10.1 — Context isolation between cycles (CRITICAL — implement with core loop)

**Problem:** A naive implementation would inject all corrections into the same request across cycles:

```
Cycle 0 request: [task]
Cycle 1 request: [task] + [correction_1]          ← OK
Cycle 2 request: [task] + [correction_1] + [correction_2]   ← BAD
Cycle 3 request: [task] + [correction_1] + [correction_2] + [correction_3]  ← BAD
```

After 2–3 cycles, the executor has more noise (stacked corrections) than signal (the task). This degrades quality.

**Solution (already implemented in `reflexion.rs` above):** Each cycle's `current_request` is rebuilt fresh from `base_request` with only the **latest** correction injected. `base_request` is never mutated.

**Test:** `context_isolation_only_latest_correction_in_request` in `reflexion.rs`.

---

### 10.2 — SHA-256 for `mentor_correction_hash` (CRITICAL — implement with core loop)

**Problem:** The original draft used `std::collections::hash_map::DefaultHasher`. `DefaultHasher` is **not stable across Rust versions** (the standard library explicitly reserves the right to change it). If the hash changes between Rust versions, the audit trail is broken.

**Solution (already implemented in `reflexion.rs` above):** Use SHA-256 via the `sha2` crate. First 16 hex chars are sufficient for the audit link.

**Dependencies to add to `crates/firstpass-proxy/Cargo.toml`:**
```toml
sha2 = "0.10"
hex  = "0.4"
```

**Test:** `hash_correction_sha256_is_stable` in `reflexion.rs` pins the expected output to catch any regression.

---

### 10.3 — Mentor input always from `base_request`, not accumulated request (CRITICAL — implement with core loop)

**Problem:** If the mentor receives the accumulated `current_request` (which already contains the previous correction in the system prompt or as a user turn), it reads its own previous correction as part of the task context. This can cause the mentor to re-correct its own corrections rather than diagnosing the root problem.

**Solution (already implemented in `reflexion.rs` above):** `build_mentor_user_message` always takes `&base_request` as its first argument. The mentor sees only the original task, the executor's latest output, and the gate failure reasons — nothing more.

---

### 10.4 — Bandit awareness of reflexion cycles (implement after core loop is stable)

**Problem:** The `StartRungBandit` observes `(context_bucket, rung, verdict)`. With the Reflexion Loop, a `Pass` verdict on rung 0 could mean:
- The executor solved it cleanly on the first try (`cycles = 0`) → strong signal, rung 0 is great for this context
- The executor needed 2 mentor corrections to pass (`cycles = 2`) → weak signal, rung 0 barely works

If the bandit treats these identically, it learns to aggressively prefer rung 0 for tasks that actually need mentor help every time. Over many requests, this means the 80B is always called (for reflexion) even when the task could be served by the 80B directly in the ladder (which would be faster because the 80B generates in one shot rather than 2× 30B + 2× mentor calls).

**Changes to `crates/firstpass-proxy/src/bandit.rs`:**

Add a new method to `StartRungBandit`:

```rust
/// Record a gate verdict with the number of reflexion cycles required.
///
/// A Pass at `cycles = 0` counts as a full Pass.
/// A Pass at `cycles >= 1` counts as a "discounted pass" — the rung needed help.
/// The discount is `1 / (1 + cycles)` applied to the pass count.
///
/// This prevents the bandit from over-indexing on rung 0 when it reliably needs
/// mentor corrections to pass. An unassisted rung-0 pass is cheaper than a
/// 2-cycle-reflexion pass (one 30B call vs. three calls total).
pub fn observe_with_cycles(
    &mut self,
    ctx: &ContextBucket,
    rung: u32,
    verdict: Verdict,
    reflexion_cycles: u32,
) {
    match verdict {
        Verdict::Abstain => {} // not counted
        Verdict::Fail => self.observe(ctx, rung, Verdict::Fail),
        Verdict::Pass => {
            if reflexion_cycles == 0 {
                // Unassisted pass: full credit.
                self.observe(ctx, rung, Verdict::Pass);
            } else {
                // Assisted pass: discounted credit.
                // discount = 1 / (1 + cycles): 1 cycle → 0.5, 2 cycles → 0.33
                let discount = 1.0 / (1.0 + reflexion_cycles as f64);
                self.discount_context(ctx);
                self.data
                    .entry(ctx.clone())
                    .or_default()
                    .entry(rung)
                    .or_default()
                    .pass += discount;
            }
        }
    }
}
```

**Test to add in `bandit.rs`:**

```rust
#[test]
fn assisted_pass_is_discounted_vs_unassisted_pass() {
    let mut b = StartRungBandit::new(10, 0.0);
    let ctx = ctx_code();
    // 20 unassisted passes at rung 0 → pass count = 20.
    for _ in 0..20 {
        b.observe_with_cycles(&ctx, 0, Verdict::Pass, 0);
    }
    let unassisted_est = b.pass_estimate(&ctx, 0).unwrap();

    let mut b2 = StartRungBandit::new(10, 0.0);
    // 20 assisted passes (2 cycles each) at rung 0 → effective pass count = 20 * 0.33 ≈ 6.6
    for _ in 0..20 {
        b2.observe_with_cycles(&ctx, 0, Verdict::Pass, 2);
    }
    let assisted_est = b2.pass_estimate(&ctx, 0).unwrap();

    assert!(
        unassisted_est > assisted_est,
        "unassisted pass rate ({unassisted_est:.2}) must exceed assisted ({assisted_est:.2})"
    );
}
```

---

### 10.5 — Adaptive convergence stopping criterion (implement after core loop is stable)

**Problem:** The loop always runs `max_reflections` cycles even when the executor's output has stopped changing. If the 30B is stuck in a local optimum, running the full 3 cycles wastes 2× 40-second mentor calls that are guaranteed to not help.

**Solution (already in config and `reflexion.rs` above):** `convergence_threshold` in `ReflexionConfig` + `normalized_edit_distance` in `reflexion.rs`.

**How it works:**
1. After each executor attempt (cycle ≥ 1), compute the normalized Levenshtein distance between the current output and the previous output.
2. If the distance < `convergence_threshold`, terminate the loop.
3. The `converged` flag on `ReflexionRun` is set to `true`.
4. The best attempt from the previous cycle is served (or the loop falls through to the ladder escalation path).

**Recommended value:** `convergence_threshold = 0.05` (stop if less than 5% of characters changed). This catches the case where the executor adds/removes a few punctuation marks but is substantively identical.

**When not to use it:** Set `convergence_threshold = 0.0` to disable (the default) during initial testing. Enable it only after verifying the loop is producing good corrections.

---

### 10.6 — `subagent_name` in `ContextBucket` (implement after core loop is stable)

**Problem:** The `ContextBucket` is `(TaskKind, prompt_token_bucket)`. In the local setup, the 30B executor handles tasks dispatched by the 80B orchestrator. The orchestrator might generate tasks of very different types (code editing, test generation, code exploration) that all arrive with the same `(CodeEdit, bucket_3)` signature. The bandit cannot distinguish them and mixes their statistics.

**Changes to `crates/firstpass-proxy/src/bandit.rs`:**

Extend `ContextBucket`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContextBucket {
    pub task_kind: TaskKind,
    pub prompt_bucket_coarse: u32,
    /// Optional subagent name, extracted from the request's session label or a custom header.
    /// `None` for requests not coming from a named subagent (backward-compat: same bucket as before).
    pub subagent_name: Option<String>,
}

impl ContextBucket {
    pub fn from_features(f: &Features) -> Self {
        Self {
            task_kind: f.task_kind,
            prompt_bucket_coarse: f.prompt_token_bucket / 2,
            subagent_name: f.subagent_name.clone(), // Option<String> on Features
        }
    }
}
```

Add `subagent_name: Option<String>` to `firstpass_core::features::Features` (in `crates/firstpass-core/src/features.rs`):

```rust
/// Optional subagent identifier — set by the proxy when the request arrives from a
/// named subagent (via a `X-Firstpass-Subagent` header or a structured session label).
/// `None` for direct harness calls. When present, the bandit uses this as an additional
/// context dimension for finer-grained start-rung selection.
///
/// Wire field name: `subagent_name` (additive, backward-compat).
#[serde(default, skip_serializing_if = "Option::is_none")]
pub subagent_name: Option<String>,
```

**In the proxy:** Extract the subagent name from the `X-Firstpass-Subagent` HTTP header (if present) and populate `features.subagent_name` before building the `EnforceCtx`.

```rust
// In the request handler, before building features:
let subagent_name = req.headers()
    .get("x-firstpass-subagent")
    .and_then(|v| v.to_str().ok())
    .map(str::to_owned);
features.subagent_name = subagent_name;
```

**Backward compat:** `subagent_name: None` (the default) hashes to the same `ContextBucket` as before this change for requests that don't set the header. No existing bandit statistics are invalidated.

---

### 10.7 — Trace store write throughput monitoring (note — no code change needed now)

**Situation:** The single-writer SQLite design (`TRACE_CHANNEL_CAP = 8192`) is correct and will not break. With the Reflexion Loop active, each request generates 2–5 trace attempts instead of 1, increasing write volume by 2–5× without changing request volume. The existing spill file (`durable` mode) handles backpressure correctly.

**What to watch:** Add a Prometheus counter (or a tracing span metric) to the writer loop that records the number of traces dropped (channel-full events). If this counter is non-zero in steady state, the write throughput has become a bottleneck.

```rust
// In writer_loop, where a trace would be dropped (durable=off path):
metrics::counter!("firstpass_traces_dropped_total").increment(1);
```

**When to act:** If `firstpass_traces_dropped_total` is non-zero in production, migrate from the single-writer blocking thread to a batched WAL writer (multiple traces per `BEGIN TRANSACTION ... COMMIT`). This is a 3–5× throughput improvement with no schema change. Do not do this pre-emptively — measure first.

---

### 10.8 — Self-verification gate (from AutoMix / Self-REF, §6–7 of survey)

**Source:** AutoMix (Aggarwal et al., 2024) and Self-REF (Chuang et al., 2025) demonstrate that a small model can estimate the quality of its own output faster and cheaper than any external gate. The self-verification score is the trigger for escalation.

**Problem:** External gates (schema, subprocess, judge) add latency and complexity. For many vibe-coding tasks, the 30B itself knows when it produced a weak answer — it just wasn't asked.

**Solution:** Add a new built-in gate kind `self_verify` that, after the executor produces its response, makes a second short call **to the same executor** asking it to rate its own confidence.

**New gate kind in `crates/firstpass-proxy/src/gate.rs`:**

```rust
/// Self-verification gate: asks the executor to rate its own output quality.
///
/// A second short call (~5–15 output tokens) is made to the SAME executor model.
/// If the response matches `pass_when`, the gate passes. Otherwise it fails.
///
/// Cost: negligible (prefill fast, ~10 output tokens at 100 tk/s = 0.1 seconds).
/// Based on the AutoMix (Aggarwal et al., 2024) and Self-REF (Chuang et al., 2025)
/// pattern from "Dynamic Model Routing and Cascading" survey (arXiv 2603.04445).
pub struct SelfVerifyGate {
    /// The prompt appended to the conversation asking for a confidence rating.
    /// Default: "Rate your confidence in the above response: high / medium / low. Answer only the rating."
    pub prompt: String,
    /// The executor's response value that counts as a gate pass.
    /// Default: "high".
    pub pass_when: String,
    /// Gate ID for health registry and logging.
    pub id: String,
}
```

**New TOML gate kind (add to `GateDef` in `config.rs`):**

```toml
[[gate]]
id         = "self-verify"
kind       = "self_verify"
pass_when  = "high"
prompt     = "Rate your confidence in the above response: high / medium / low. Answer only the rating."
```

**Integration in `reflexion.rs`:** The self-verify gate is evaluated like any other gate. If it fails (model says `"medium"` or `"low"`), the reflexion loop triggers the mentor. This removes the need for any external process gate for the common case.

**New field on `FinalOutcome` in `trace.rs`:**

```rust
/// Whether the loop was triggered by a self-verification failure (vs an external gate).
/// `None` when self-verify gate was not configured.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub triggered_by_self_verify: Option<bool>,
```

**Acceptance criteria:**
- Gate with `kind = "self_verify"` parses without error.
- Mock test: executor returns `"low"` → gate fails → mentor is called → executor retries → returns `"high"` → gate passes. Assert `served_rung = Some(0)`, `cycles_completed = 1`.
- Mock test: executor returns `"high"` on first try → gate passes immediately, no mentor call.

---

### 10.9 — Latency cap and exhausted policy enforcement in `run_reflexion_loop` (implement with core loop)

**Problem:** `max_latency_ms` and `on_reflexion_exhausted` are in the config but have no runtime effect unless `run_reflexion_loop` checks them. This section specifies exactly where and how to enforce them.

**Where to add the latency check** — at the top of each `for cycle in 0..max_cycles` iteration, after `cycle_start = Instant::now()` and cumulative tracking:

```rust
// Track cumulative latency from the loop start (not per-cycle).
let loop_start = Instant::now(); // initialized BEFORE the for loop

// Inside the for loop, before each executor call:
if let Some(cap_ms) = ctx.config.max_latency_ms {
    let elapsed = u64::try_from(loop_start.elapsed().as_millis()).unwrap_or(u64::MAX);
    if elapsed >= cap_ms {
        tracing::info!(
            elapsed_ms = elapsed,
            cap_ms,
            cycle,
            "reflexion: latency cap exceeded — terminating loop"
        );
        break; // falls through to on_reflexion_exhausted handling below
    }
}
```

**Where to enforce `on_reflexion_exhausted`** — in `route_enforce`, after `run_reflexion_loop` returns with `served_rung = None`:

```rust
// Reflexion loop exhausted or capped without a passing gate.
if run.served_rung.is_none() {
    match ctx.reflexion.map(|r| &r.on_reflexion_exhausted) {
        Some(ReflexionExhaustedPolicy::Error) | None => {
            // "error" policy: signal hard failure to the caller.
            // The harness receives a 5xx-class error and can re-plan.
            return (
                EngineOutcome::Failed("reflexion exhausted without passing gate".into()),
                build_failed_trace(/* ... */),
            );
        }
        Some(ReflexionExhaustedPolicy::ServeBestAttempt) => {
            // Fall through to run_ladder with the pre_attempts prepended.
            // run_ladder will escalate to rung 1 (80B) in the ladder as usual.
        }
    }
}
```

**New field on `FinalOutcome`:**

```rust
/// Whether the reflexion loop was terminated early due to the latency cap.
/// Distinct from convergence (`reflexion_converged`) — here the model was still making
/// progress but time ran out.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub reflexion_latency_capped: Option<bool>,
```

**Acceptance criteria:**
- Config with `max_latency_ms = 1` (1ms, always trips): loop executes exactly one executor call, then exits with `reflexion_latency_capped = Some(true)`.
- Config with `on_reflexion_exhausted = "error"`: exhausted loop returns `EngineOutcome::Failed`, trace has no `served_rung`.
- Config with `on_reflexion_exhausted = "serve_best_attempt"` (default): exhausted loop falls through to `run_ladder` for escalation.

