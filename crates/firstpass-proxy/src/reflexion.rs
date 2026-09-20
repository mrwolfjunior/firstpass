//! Mentor-guided Reflexion Loop (SPEC §4, §10.1–10.3, §10.5, §10.9).
//!
//! When a gate fails on the executor rung, the mentor model reads the task context,
//! the executor's output, and the failure reasons, and produces a compact targeted correction.
//! The executor retries with this correction note injected. Repeats up to
//! `ReflexionConfig::max_reflections` times before the request is considered exhausted.
//!
//! # Context Isolation Invariant (§10.1, §10.3)
//! Only the LATEST mentor correction is ever injected into the executor's request.
//! Each cycle builds the request fresh from `base_request` (which is never mutated).
//! The mentor prompt is also always derived from `base_request`, not the accumulated request.
//!
//! # Deterministic Audit Hashing (§10.2)
//! All mentor corrections are hashed using SHA-256 (first 16 hex chars) to guarantee
//! version-stable, re-derivable audit trails for external verification.

use std::time::Instant;

use crate::gate::{Gate, GateHealthRegistry, aggregate_with_policy};
use crate::provider::{Auth, ModelRequest, ModelResponse, ProviderRegistry};
use firstpass_core::config::ReflexionConfig;
use firstpass_core::verdict::reason;
use firstpass_core::{Attempt, GateResult, ModelRef, PriceTable, Verdict};

/// Output of a completed reflexion loop run.
#[derive(Debug)]
pub struct ReflexionRun {
    pub attempts: Vec<Attempt>,
    pub executor_cost_usd: f64,
    pub mentor_cost_usd: f64,
    pub gate_cost_total: f64,
    pub best: Option<(usize, ModelResponse)>, // (attempt_index, response)
    pub served_rung: Option<u32>,
    pub cycles_completed: u32,
    pub converged: bool,
    pub hard_error: Option<String>,
    pub latency_capped: bool, // true if max_latency_ms was hit
}

/// Dependencies injected into the reflexion loop. All borrows, no ownership.
#[derive(Debug)]
pub struct ReflexionCtx<'a> {
    pub config: &'a ReflexionConfig,
    pub executor_rung: u32,
    pub executor_model: &'a str,
    pub gates: &'a [Box<dyn Gate>],
    pub health: &'a GateHealthRegistry,
    pub base_request: &'a ModelRequest, // NEVER mutate this
    pub providers: &'a ProviderRegistry,
    pub auth: Option<&'a Auth>,
    pub prices: &'a PriceTable,
    pub tenant_id: &'a str,
    pub serve_threshold: Option<f64>,
}

const DEFAULT_MENTOR_SYSTEM_PROMPT: &str = "\
You are a senior code reviewer. \
Read the task, the executor's output, and the gate failure reasons. \
Identify the root cause of the failure and emit a short targeted correction. \
Output JSON only: {\"correction\": \"...\", \"strategy\": \"...\"}. \
Do NOT rewrite the full answer. Maximum 200 tokens.";

/// SHA-256 of the correction text, hex-encoded, first 16 chars.
///
/// Deterministic, version-stable, and verifiable by external auditors.
/// Uses `sha2` and `hex` crates (never `DefaultHasher`).
fn hash_correction_sha256(correction: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(correction.as_bytes());
    let hex_str = hex::encode(digest);
    hex_str[..16].to_owned()
}

/// Normalized edit distance between two strings in `[0, 1]`.
///
/// Levenshtein distance normalized by `max(len(a), len(b))`.
/// Returns 0.0 if both strings are empty, 1.0 if one is empty and the other is not.
/// O(n·m) time, O(min(n, m)) space (two-row rolling array over bytes).
fn normalized_edit_distance(a: &str, b: &str) -> f64 {
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    let na = a_bytes.len();
    let nb = b_bytes.len();
    if na == 0 && nb == 0 {
        return 0.0;
    }
    if na == 0 || nb == 0 {
        return 1.0;
    }
    let max_len = na.max(nb);

    // Keep the smaller slice as second dimension to achieve O(min(n, m)) space.
    let (a_slice, b_slice) = if na < nb {
        (b_bytes, a_bytes)
    } else {
        (a_bytes, b_bytes)
    };
    let nb = b_slice.len();

    let mut prev: Vec<usize> = (0..=nb).collect();
    let mut curr = vec![0usize; nb + 1];

    for (i, &ca) in a_slice.iter().enumerate() {
        curr[0] = i + 1;
        for (j, &cb) in b_slice.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[nb] as f64 / max_len as f64
}

/// Parse the mentor's JSON response to extract the `correction` string.
///
/// Tolerates leading preamble text before the JSON object.
fn parse_mentor_correction(raw: &str) -> Option<String> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
        return v
            .get("correction")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
    }
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end <= start {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&raw[start..=end]).ok()?;
    v.get("correction")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// Format the compact mentor user message.
///
/// ALWAYS extracts the last user-role message from `base_req` (never accumulated request),
/// truncates executor output to 1000 chars, and formats with gate failure reasons.
fn build_mentor_user_message(
    base_req: &ModelRequest,
    resp: &ModelResponse,
    failure_reasons: &[&str],
) -> String {
    let last_user = base_req
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(crate::provider::ChatMessage::text_view)
        .unwrap_or_else(|| "<no user message>".to_owned());

    const MAX_OUTPUT_CHARS: usize = 1000;
    let executor_out = if resp.text.chars().count() > MAX_OUTPUT_CHARS {
        let truncated: String = resp.text.chars().take(MAX_OUTPUT_CHARS).collect();
        let remaining = resp.text.chars().count() - MAX_OUTPUT_CHARS;
        format!("{truncated}… [truncated {remaining} chars]")
    } else {
        resp.text.clone()
    };

    let failures = if failure_reasons.is_empty() {
        "- (gate abstained or unknown reason)".to_owned()
    } else {
        failure_reasons
            .iter()
            .map(|r| format!("- {r}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!(
        "=== TASK ===\n{last_user}\n\n=== EXECUTOR OUTPUT ===\n{executor_out}\n\n=== GATE FAILURES ===\n{failures}"
    )
}

/// Inject the latest correction into a new clone of `base_req`.
///
/// Returns a new `ModelRequest` without mutating `base_req`.
/// When `as_user_turn` is true, appends a user message with the correction note.
/// When false, prepends a system note to `req.system`.
fn inject_correction(
    base_req: &ModelRequest,
    correction: &str,
    as_user_turn: bool,
) -> ModelRequest {
    let mut req = base_req.clone();
    if as_user_turn {
        req.messages.push(crate::provider::ChatMessage::text(
            "user",
            format!("[MENTOR NOTE — do not quote in your response]: {correction}"),
        ));
    } else {
        let note = format!(
            "[MENTOR CORRECTION]: {correction}\n\nPlease address the above before responding.\n\n"
        );
        match &mut req.system {
            Some(s) => s.insert_str(0, &note),
            None => req.system = Some(note),
        }
    }
    req
}

/// Construct an abstain attempt on hard error or failover-eligible provider error.
#[allow(clippy::too_many_arguments)]
fn make_abstain_attempt(
    rung: u32,
    model: &str,
    provider: &str,
    reason_str: &str,
    ms: u64,
    reflexion_cycle: Option<u32>,
    mentor_correction_hash: Option<String>,
    reflexion_converged: Option<bool>,
) -> Attempt {
    Attempt {
        rung,
        model: model.to_owned(),
        provider: provider.to_owned(),
        in_tokens: 0,
        cache_write_tokens: 0,
        cache_read_tokens: 0,
        out_tokens: 0,
        cost_usd: 0.0,
        latency_ms: ms,
        gates: vec![GateResult::abstain(provider, reason_str, ms)],
        verdict: Verdict::Abstain,
        reflexion_cycle,
        mentor_correction_hash,
        reflexion_converged,
    }
}

/// Run the reflexion loop for the executor rung.
///
/// # Invariants
/// - Cumulative wall-clock latency is checked at start of cycles (cycle > 0) and before mentor calls.
/// - Each cycle starts fresh from `base_request` with only the latest correction injected.
/// - Output convergence terminates early if normalized edit distance drops below threshold.
/// - All mentor corrections are hashed with SHA-256 for the tamper-evident audit trace.
pub async fn run_reflexion_loop(ctx: &ReflexionCtx<'_>) -> ReflexionRun {
    let loop_start = Instant::now();
    let mut attempts: Vec<Attempt> = Vec::new();
    let mut executor_cost_usd = 0.0_f64;
    let mut mentor_cost_usd = 0.0_f64;
    let mut gate_cost_total = 0.0_f64;
    let mut best: Option<(usize, ModelResponse)> = None;
    let mut served_rung: Option<u32> = None;
    let mut cycles_completed = 0u32;
    let mut converged = false;
    let mut latency_capped = false;
    let mut latest_correction: Option<String> = None;
    let mut pending_mentor_correction_hash: Option<String> = None;
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
            latency_capped,
        };
    };

    let default_auth = Auth::default();
    let auth = ctx.auth.unwrap_or(&default_auth);

    let max_cycles = ctx.config.max_reflections + 1;

    for cycle in 0..max_cycles {
        // Latency cap check at cycle boundary (cycle > 0)
        if cycle > 0
            && let Some(cap_ms) = ctx.config.max_latency_ms
        {
            let elapsed = loop_start.elapsed().as_millis() as u64;
            if elapsed >= cap_ms {
                latency_capped = true;
                break;
            }
        }

        // Rebuild current request fresh from base_request with latest correction only
        let mut current_request = if let Some(ref c) = latest_correction {
            inject_correction(ctx.base_request, c, ctx.config.inject_as_user_turn)
        } else {
            ctx.base_request.clone()
        };
        current_request.model = ctx.executor_model.to_owned();

        // Call executor
        let cycle_start = Instant::now();
        let resp = match executor_provider.complete(&current_request, auth).await {
            Err(err) if err.is_failover_eligible() => {
                let ms = cycle_start.elapsed().as_millis() as u64;
                attempts.push(make_abstain_attempt(
                    ctx.executor_rung,
                    ctx.executor_model,
                    executor_provider.id(),
                    reason::PROVIDER_ERROR,
                    ms,
                    if cycle > 0 { Some(cycle) } else { None },
                    pending_mentor_correction_hash.take(),
                    None,
                ));
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
                        "reflexion: executor provider error on cycle {cycle}"
                    )),
                    latency_capped,
                };
            }
            Err(err) => {
                let ms = cycle_start.elapsed().as_millis() as u64;
                attempts.push(make_abstain_attempt(
                    ctx.executor_rung,
                    ctx.executor_model,
                    executor_provider.id(),
                    reason::PROVIDER_ERROR,
                    ms,
                    if cycle > 0 { Some(cycle) } else { None },
                    pending_mentor_correction_hash.take(),
                    None,
                ));
                return ReflexionRun {
                    attempts,
                    executor_cost_usd,
                    mentor_cost_usd,
                    gate_cost_total,
                    best,
                    served_rung,
                    cycles_completed,
                    converged,
                    hard_error: Some(format!("reflexion: executor hard error: {err}")),
                    latency_capped,
                };
            }
            Ok(r) => r,
        };

        let ms = cycle_start.elapsed().as_millis() as u64;
        let model_cost = ctx
            .prices
            .cost_usd_with_cache(
                ctx.executor_model,
                resp.in_tokens,
                resp.cache_write_tokens,
                resp.cache_read_tokens,
                resp.out_tokens,
            )
            .unwrap_or(0.0);
        executor_cost_usd += model_cost;

        // Convergence check: compare against PREVIOUS CYCLE's output (not best)
        if cycle >= 1
            && ctx.config.convergence_threshold > 0.0
            && let Some(ref prev) = prev_output
            && normalized_edit_distance(prev, &resp.text) < ctx.config.convergence_threshold
        {
            tracing::info!(
                cycle,
                threshold = ctx.config.convergence_threshold,
                "reflexion: convergence detected — stopping loop early"
            );
            converged = true;
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
                verdict: Verdict::Abstain,
                reflexion_cycle: Some(cycle),
                mentor_correction_hash: pending_mentor_correction_hash.take(),
                reflexion_converged: Some(true),
            });
            // Do NOT update best — keep the previous best attempt
            break;
        }

        // Update prev_output EVERY cycle regardless of verdict
        prev_output = Some(resp.text.clone());

        // Evaluate gates
        let mut gate_results: Vec<GateResult> = Vec::with_capacity(ctx.gates.len());
        let fail_closed: std::collections::HashSet<&str> = ctx
            .gates
            .iter()
            .filter(|g| g.abstain_fails_closed())
            .map(|g| g.id())
            .collect();

        for g in ctx.gates {
            if !ctx.health.enabled(ctx.tenant_id, g.id()) {
                tracing::warn!(gate = %g.id(), cycle, "skipping auto-disabled gate (reflexion)");
                continue;
            }
            let r = g.evaluate(&current_request, &resp).await;
            ctx.health
                .record(ctx.tenant_id, g.id(), r.verdict == Verdict::Abstain);
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

        let attempt_idx = attempts.len();
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

        // Update best attempt (prefer Pass > Abstain > Fail)
        let rank = |v: Verdict| match v {
            Verdict::Pass => 3,
            Verdict::Abstain => 2,
            Verdict::Fail => 1,
        };
        let prev_rank = best
            .as_ref()
            .map(|(i, _)| rank(attempts[*i].verdict))
            .unwrap_or(0);
        if rank(verdict) >= prev_rank {
            best = Some((attempt_idx, resp.clone()));
        }

        if serve {
            served_rung = Some(ctx.executor_rung);
            break;
        }

        if cycle + 1 >= max_cycles {
            break;
        }

        // Latency cap check before calling mentor: avoid starting an expensive mentor call if budget is hit
        if let Some(cap_ms) = ctx.config.max_latency_ms {
            let elapsed = loop_start.elapsed().as_millis() as u64;
            if elapsed >= cap_ms {
                latency_capped = true;
                break;
            }
        }

        // Call mentor
        let mentor_provider = match ModelRef::parse(&ctx.config.mentor_model) {
            Ok(m) => ctx.providers.get(&m.provider),
            Err(_) => None,
        };
        let Some(mentor_prov) = mentor_provider else {
            tracing::warn!(
                mentor_model = %ctx.config.mentor_model,
                "reflexion: mentor provider not found; skipping reflection"
            );
            break;
        };

        let failure_reasons: Vec<&str> = gate_results
            .iter()
            .filter(|g| g.verdict == Verdict::Fail)
            .filter_map(|g| g.reason.as_deref())
            .collect();

        let mentor_user_content =
            build_mentor_user_message(ctx.base_request, &resp, &failure_reasons);

        let system_prompt = ctx
            .config
            .mentor_system_prompt
            .as_deref()
            .unwrap_or(DEFAULT_MENTOR_SYSTEM_PROMPT);

        let mentor_req = ModelRequest {
            model: ctx.config.mentor_model.clone(),
            system: Some(system_prompt.to_owned()),
            messages: vec![crate::provider::ChatMessage::text(
                "user",
                mentor_user_content,
            )],
            max_tokens: ctx.config.mentor_max_out_tokens,
            tools: serde_json::Value::Null,
            raw: serde_json::Value::Null,
            cache_prefix: false,
        };

        let correction = match mentor_prov.complete(&mentor_req, auth).await {
            Err(e) => {
                tracing::warn!(error = %e, cycle, "reflexion: mentor call failed; ending loop");
                break;
            }
            Ok(mr) => {
                let mentor_model_cost = ctx
                    .prices
                    .cost_usd_with_cache(
                        &ctx.config.mentor_model,
                        mr.in_tokens,
                        mr.cache_write_tokens,
                        mr.cache_read_tokens,
                        mr.out_tokens,
                    )
                    .unwrap_or(0.0);
                mentor_cost_usd += mentor_model_cost;
                parse_mentor_correction(&mr.text).unwrap_or(mr.text)
            }
        };

        pending_mentor_correction_hash = Some(hash_correction_sha256(&correction));
        latest_correction = Some(correction);
        cycles_completed += 1;
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
        hard_error: None,
        latency_capped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    // ── 1. parse_mentor_correction_valid_json ────────────────────────────────
    #[test]
    fn parse_mentor_correction_valid_json() {
        let raw = r#"{"correction": "fix X", "strategy": "try Y"}"#;
        assert_eq!(parse_mentor_correction(raw), Some("fix X".to_owned()));
    }

    // ── 2. parse_mentor_correction_with_preamble ─────────────────────────────
    #[test]
    fn parse_mentor_correction_with_preamble() {
        let raw = r#"Here is the JSON: {"correction": "abc"}"#;
        assert_eq!(parse_mentor_correction(raw), Some("abc".to_owned()));
    }

    // ── 3. parse_mentor_correction_invalid_returns_none ──────────────────────
    #[test]
    fn parse_mentor_correction_invalid_returns_none() {
        assert_eq!(parse_mentor_correction("not json"), None);
        assert_eq!(parse_mentor_correction(r#"{"other_key": "val"}"#), None);
    }

    // ── 4. inject_as_system_prefix ───────────────────────────────────────────
    #[test]
    fn inject_as_system_prefix() {
        let base = make_test_request();
        let injected = inject_correction(&base, "fix your formatting", false);
        assert!(
            injected
                .system
                .as_deref()
                .unwrap_or("")
                .starts_with("[MENTOR CORRECTION]")
        );
        assert!(
            injected
                .system
                .as_deref()
                .unwrap_or("")
                .contains("fix your formatting")
        );
    }

    // ── 5. inject_as_user_turn ───────────────────────────────────────────────
    #[test]
    fn inject_as_user_turn() {
        let base = make_test_request();
        let injected = inject_correction(&base, "fix your formatting", true);
        assert_eq!(injected.messages.len(), base.messages.len() + 1);
        let last = injected.messages.last().unwrap();
        assert_eq!(last.role, "user");
        assert!(last.text_view().contains("[MENTOR NOTE"));
        assert!(last.text_view().contains("fix your formatting"));
    }

    // ── 6. hash_correction_sha256_is_stable ──────────────────────────────────
    #[test]
    fn hash_correction_sha256_is_stable() {
        let h = hash_correction_sha256("hello");
        // SHA-256("hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
        // First 16 hex chars:
        assert_eq!(h, "2cf24dba5fb0a30e");
        // Calling again must return the same value (no randomness).
        assert_eq!(hash_correction_sha256("hello"), "2cf24dba5fb0a30e");
    }

    // ── 7. normalized_edit_distance_identical ────────────────────────────────
    #[test]
    fn normalized_edit_distance_identical() {
        assert_eq!(normalized_edit_distance("abc", "abc"), 0.0);
    }

    // ── 8. normalized_edit_distance_completely_different ─────────────────────
    #[test]
    fn normalized_edit_distance_completely_different() {
        let dist = normalized_edit_distance("abc", "xyz");
        assert!(dist > 0.5, "expected > 0.5, got {dist}");
        assert_eq!(dist, 1.0);
    }

    // ── 9. normalized_edit_distance_empty ────────────────────────────────────
    #[test]
    fn normalized_edit_distance_empty() {
        assert_eq!(normalized_edit_distance("", ""), 0.0);
        assert_eq!(normalized_edit_distance("", "x"), 1.0);
        assert_eq!(normalized_edit_distance("x", ""), 1.0);
    }

    // ── Extra: partial distance ──────────────────────────────────────────────
    #[test]
    fn normalized_edit_distance_partial_change() {
        let d = normalized_edit_distance("abcde", "abxde");
        assert!((d - 0.2).abs() < 1e-9, "expected ~0.2, got {d}");
    }

    // ── 10. context_isolation_inject_uses_base_not_accumulated ───────────────
    #[test]
    fn context_isolation_inject_uses_base_not_accumulated() {
        // Build a base request with one user message.
        let base = make_test_request();

        // Inject correction_1 -> req_1 (system prefix mode)
        let req_1 = inject_correction(&base, "correction_1", false);
        // Inject correction_2 -> req_2 from the SAME base (not from req_1)
        let req_2 = inject_correction(&base, "correction_2", false);

        // req_1 must contain correction_1 and NOT correction_2 in system prompt
        let sys_1 = req_1.system.as_deref().unwrap_or("");
        assert!(
            sys_1.contains("correction_1"),
            "req_1 must contain correction_1"
        );
        assert!(
            !sys_1.contains("correction_2"),
            "req_1 must NOT contain correction_2"
        );

        // req_2 must contain correction_2 and NOT correction_1 in system prompt
        let sys_2 = req_2.system.as_deref().unwrap_or("");
        assert!(
            sys_2.contains("correction_2"),
            "req_2 must contain correction_2"
        );
        assert!(
            !sys_2.contains("correction_1"),
            "req_2 must NOT contain correction_1"
        );

        // The base must be unchanged
        let base_sys = base.system.as_deref().unwrap_or("");
        assert!(
            !base_sys.contains("correction_1"),
            "base must not be mutated"
        );
        assert!(
            !base_sys.contains("correction_2"),
            "base must not be mutated"
        );

        // Same for user-turn mode
        let req_u1 = inject_correction(&base, "correction_u1", true);
        let req_u2 = inject_correction(&base, "correction_u2", true);
        let ru1 = format!("{:?}", req_u1.messages);
        let ru2 = format!("{:?}", req_u2.messages);
        assert!(
            ru1.contains("correction_u1") && !ru1.contains("correction_u2"),
            "req_u1 must contain correction_u1 and NOT correction_u2"
        );
        assert!(
            ru2.contains("correction_u2") && !ru2.contains("correction_u1"),
            "req_u2 must contain correction_u2 and NOT correction_u1"
        );

        let base_msgs = format!("{:?}", base.messages);
        assert!(
            !base_msgs.contains("correction_u1"),
            "base must not be mutated"
        );
        assert!(
            !base_msgs.contains("correction_u2"),
            "base must not be mutated"
        );
    }

    // ── 11. latency_cap_zero_stops_after_first_cycle ─────────────────────────
    #[tokio::test]
    async fn latency_cap_zero_stops_after_first_cycle() {
        let config = firstpass_core::config::ReflexionConfig {
            mentor_model: "mentor/m".to_owned(),
            max_reflections: 2,
            mentor_max_out_tokens: 200,
            mentor_system_prompt: None,
            inject_as_user_turn: false,
            convergence_threshold: 0.0,
            max_latency_ms: Some(0),
            on_reflexion_exhausted:
                firstpass_core::config::ReflexionExhaustedPolicy::ServeBestAttempt,
        };

        let executor = Arc::new(MockTestProvider::new(
            "mock-exec",
            vec![make_test_response("bad output")],
        ));
        let mentor = Arc::new(MockTestProvider::new(
            "mentor",
            vec![make_test_response(r#"{"correction": "fix it"}"#)],
        ));

        let mut providers_map: HashMap<String, Arc<dyn crate::provider::Provider>> = HashMap::new();
        providers_map.insert("mock-exec".to_owned(), executor);
        providers_map.insert("mentor".to_owned(), mentor);
        let providers = ProviderRegistry::from_map(providers_map);

        let gate: Box<dyn Gate> = Box::new(MockTestGate {
            id: "failing-gate".to_owned(),
            verdict: Verdict::Fail,
        });
        let gates = vec![gate];
        let health = GateHealthRegistry::new();
        let base_req = make_test_request();
        let prices = make_test_prices();

        let ctx = ReflexionCtx {
            config: &config,
            executor_rung: 0,
            executor_model: "mock-exec/m",
            gates: &gates,
            health: &health,
            base_request: &base_req,
            providers: &providers,
            auth: None,
            prices: &prices,
            tenant_id: "test-tenant",
            serve_threshold: None,
        };

        let run = run_reflexion_loop(&ctx).await;
        assert!(run.latency_capped, "expected latency_capped to be true");
        assert_eq!(run.cycles_completed, 0, "expected 0 cycles completed");
        assert_eq!(run.attempts.len(), 1, "expected exactly 1 attempt");
    }

    // ── 12. convergence_stops_loop_early ─────────────────────────────────────
    #[tokio::test]
    async fn convergence_stops_loop_early() {
        let config = firstpass_core::config::ReflexionConfig {
            mentor_model: "mentor/m".to_owned(),
            max_reflections: 3,
            mentor_max_out_tokens: 200,
            mentor_system_prompt: None,
            inject_as_user_turn: false,
            convergence_threshold: 0.01,
            max_latency_ms: None,
            on_reflexion_exhausted:
                firstpass_core::config::ReflexionExhaustedPolicy::ServeBestAttempt,
        };

        // Executor returns identical output twice: "same output"
        let executor = Arc::new(MockTestProvider::new(
            "mock-exec",
            vec![
                make_test_response("same output"),
                make_test_response("same output"),
            ],
        ));
        let mentor = Arc::new(MockTestProvider::new(
            "mentor",
            vec![
                make_test_response(r#"{"correction": "try something else"}"#),
                make_test_response(r#"{"correction": "try again"}"#),
            ],
        ));

        let mut providers_map: HashMap<String, Arc<dyn crate::provider::Provider>> = HashMap::new();
        providers_map.insert("mock-exec".to_owned(), executor);
        providers_map.insert("mentor".to_owned(), mentor);
        let providers = ProviderRegistry::from_map(providers_map);

        let gate: Box<dyn Gate> = Box::new(MockTestGate {
            id: "failing-gate".to_owned(),
            verdict: Verdict::Fail,
        });
        let gates = vec![gate];
        let health = GateHealthRegistry::new();
        let base_req = make_test_request();
        let prices = make_test_prices();

        let ctx = ReflexionCtx {
            config: &config,
            executor_rung: 0,
            executor_model: "mock-exec/m",
            gates: &gates,
            health: &health,
            base_request: &base_req,
            providers: &providers,
            auth: None,
            prices: &prices,
            tenant_id: "test-tenant",
            serve_threshold: None,
        };

        let run = run_reflexion_loop(&ctx).await;
        assert!(run.converged, "expected loop to converge");
        assert!(
            run.cycles_completed < config.max_reflections,
            "expected loop to terminate before exhausting reflections"
        );
        assert_eq!(
            run.attempts.len(),
            2,
            "expected initial attempt + 1 converged retry"
        );
        assert_eq!(run.attempts[1].reflexion_converged, Some(true));
        // best must remain the earlier valid attempt (attempt 0), not overwritten by the converged attempt
        assert_eq!(run.best.as_ref().map(|(idx, _)| *idx), Some(0));
    }

    // ── Extra: convergence compares against immediately preceding cycle ──────
    #[tokio::test]
    async fn convergence_compares_against_immediately_preceding_cycle() {
        let config = firstpass_core::config::ReflexionConfig {
            mentor_model: "mentor/m".to_owned(),
            max_reflections: 3,
            mentor_max_out_tokens: 200,
            mentor_system_prompt: None,
            inject_as_user_turn: false,
            convergence_threshold: 0.01,
            max_latency_ms: None,
            on_reflexion_exhausted:
                firstpass_core::config::ReflexionExhaustedPolicy::ServeBestAttempt,
        };

        // Cycle 0: "output A"
        // Cycle 1: "output B" (different from A, does not converge)
        // Cycle 2: "output B" (identical to cycle 1 -> converges!)
        let executor = Arc::new(MockTestProvider::new(
            "mock-exec",
            vec![
                make_test_response("output A"),
                make_test_response("output B"),
                make_test_response("output B"),
            ],
        ));
        let mentor = Arc::new(MockTestProvider::new(
            "mentor",
            vec![
                make_test_response(r#"{"correction": "try something else"}"#),
                make_test_response(r#"{"correction": "try again"}"#),
            ],
        ));

        let mut providers_map: HashMap<String, Arc<dyn crate::provider::Provider>> = HashMap::new();
        providers_map.insert("mock-exec".to_owned(), executor);
        providers_map.insert("mentor".to_owned(), mentor);
        let providers = ProviderRegistry::from_map(providers_map);

        let gate: Box<dyn Gate> = Box::new(MockTestGate {
            id: "failing-gate".to_owned(),
            verdict: Verdict::Fail,
        });
        let gates = vec![gate];
        let health = GateHealthRegistry::new();
        let base_req = make_test_request();
        let prices = make_test_prices();

        let ctx = ReflexionCtx {
            config: &config,
            executor_rung: 0,
            executor_model: "mock-exec/m",
            gates: &gates,
            health: &health,
            base_request: &base_req,
            providers: &providers,
            auth: None,
            prices: &prices,
            tenant_id: "test-tenant",
            serve_threshold: None,
        };

        let run = run_reflexion_loop(&ctx).await;
        assert!(run.converged, "expected loop to converge on cycle 2");
        assert_eq!(
            run.attempts.len(),
            3,
            "expected 2 normal attempts + 1 converged attempt"
        );
        assert_eq!(run.attempts[2].reflexion_converged, Some(true));
        // best should not be the unverified converged attempt 2
        assert_ne!(run.best.as_ref().map(|(idx, _)| *idx), Some(2));
    }

    // ── Test helpers ─────────────────────────────────────────────────────────

    #[derive(Debug)]
    struct MockTestProvider {
        id: String,
        responses: Mutex<Vec<ModelResponse>>,
    }

    impl MockTestProvider {
        fn new(id: impl Into<String>, responses: Vec<ModelResponse>) -> Self {
            Self {
                id: id.into(),
                responses: Mutex::new(responses),
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::provider::Provider for MockTestProvider {
        fn id(&self) -> &str {
            &self.id
        }
        fn carries_structured_verbatim(&self, _inbound: firstpass_core::Dialect) -> bool {
            true
        }
        async fn complete(
            &self,
            _req: &ModelRequest,
            _auth: &Auth,
        ) -> Result<ModelResponse, crate::provider::ProviderError> {
            let mut list = self.responses.lock().unwrap();
            if list.is_empty() {
                Ok(make_test_response("default response"))
            } else {
                Ok(list.remove(0))
            }
        }
    }

    #[derive(Debug)]
    struct MockTestGate {
        id: String,
        verdict: Verdict,
    }

    #[async_trait::async_trait]
    impl Gate for MockTestGate {
        fn id(&self) -> &str {
            &self.id
        }
        async fn evaluate(&self, _req: &ModelRequest, _resp: &ModelResponse) -> GateResult {
            GateResult::deterministic(&self.id, self.verdict, 10)
        }
    }

    fn make_test_request() -> ModelRequest {
        ModelRequest {
            model: "mock-exec/m".to_owned(),
            system: Some("System prompt".to_owned()),
            messages: vec![crate::provider::ChatMessage::text(
                "user",
                "User task prompt",
            )],
            max_tokens: 100,
            tools: serde_json::Value::Null,
            raw: serde_json::Value::Null,
            cache_prefix: false,
        }
    }

    fn make_test_response(text: &str) -> ModelResponse {
        ModelResponse {
            model: "mock-exec/m".to_owned(),
            text: text.to_owned(),
            in_tokens: 100,
            cache_write_tokens: 0,
            cache_read_tokens: 0,
            out_tokens: 50,
            raw: serde_json::Value::Null,
        }
    }

    fn make_test_prices() -> PriceTable {
        PriceTable::new()
            .with_override(
                "mock-exec/m",
                firstpass_core::cost::ModelPrice {
                    input_per_mtok: 0.1,
                    output_per_mtok: 0.2,
                },
            )
            .with_override(
                "mentor/m",
                firstpass_core::cost::ModelPrice {
                    input_per_mtok: 0.5,
                    output_per_mtok: 1.0,
                },
            )
    }
}
