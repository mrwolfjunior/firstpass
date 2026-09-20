# Implementation Plan: Reflexion Loop (v2)

**Spec:** [`docs/superpowers/specs/2026-09-19-reflexion-loop-local-design.md`](../specs/2026-09-19-reflexion-loop-local-design.md)  
**Research:** [`docs/superpowers/research/2026-09-20-arxiv-routing-survey-analysis.md`](../research/2026-09-20-arxiv-routing-survey-analysis.md)  
**Date:** 2026-09-20

> **Regola d'oro:** ogni fase termina con `cargo test --workspace` + `cargo clippy --workspace --all-targets -- -D warnings` che passano. Non si procede alla fase successiva finché i check non sono verdi.

---

## Phase 1 — Core schema & config (`firstpass-core`)

*Obiettivo: estendere il contratto di configurazione e il trace schema senza rompere nulla. Nessun I/O, nessuna logica di loop.*

### 1.1 `features.rs`
- [ ] Aggiungere `#[serde(default, skip_serializing_if = "Option::is_none")] pub subagent_name: Option<String>` alla struct `Features`.

### 1.2 `config.rs`
- [ ] Definire `ReflexionExhaustedPolicy` enum (`ServeBestAttempt` | `Error`) con `#[serde(rename_all = "snake_case")]`.
- [ ] Definire `ReflexionConfig` struct con `#[serde(deny_unknown_fields)]` e tutti i campi:
  - `mentor_model: String`
  - `max_reflections: u32` (default 2)
  - `mentor_max_out_tokens: u32` (default 200)
  - `mentor_system_prompt: Option<String>`
  - `inject_as_user_turn: bool`
  - `convergence_threshold: f64` (default 0.0)
  - `max_latency_ms: Option<u64>`
  - `on_reflexion_exhausted: ReflexionExhaustedPolicy` (default `ServeBestAttempt`)
- [ ] Aggiungere `pub reflexion: Option<ReflexionConfig>` alla struct `Config`.
- [ ] Aggiungere `pub reflexion: Option<ReflexionConfig>` alla struct `Route`.
- [ ] Aggiungere la validazione in `Config::parse`:
  - `max_reflections` in `[1, 5]`
  - `mentor_max_out_tokens > 0`
  - `convergence_threshold` in `[0.0, 1.0)`

### 1.3 `trace.rs`

**Su `Attempt`** (tutti `#[serde(default, skip_serializing_if = "Option::is_none")]`):
- [ ] `reflexion_cycle: Option<u32>`
- [ ] `mentor_correction_hash: Option<String>`
- [ ] `reflexion_converged: Option<bool>`

**Su `FinalOutcome`** (tutti `#[serde(default, skip_serializing_if = "Option::is_none")]`):
- [ ] `reflexion_cycles: Option<u32>`
- [ ] `mentor_cost_usd: Option<f64>`
- [ ] `reflexion_cycles_to_pass: Option<u32>`
- [ ] `triggered_by_self_verify: Option<bool>`
- [ ] `reflexion_latency_capped: Option<bool>`

### 1.4 Verifica Fase 1
```bash
cargo test -p firstpass-core
cargo clippy -p firstpass-core --all-targets -- -D warnings
```
- [ ] I test esistenti passano (hash chain, serde backward-compat).
- [ ] Un `Trace` con tutti i nuovi campi `= None` serializza byte-identicamente a un trace pre-reflexion (test da aggiungere).

---

## Phase 2 — Motore reflexion (`crates/firstpass-proxy/src/reflexion.rs`)

*Obiettivo: creare il file `reflexion.rs` con tutta la logica del loop. Nessuna modifica ai file esistenti in questa fase.*

### 2.1 Dipendenze
- [ ] Aggiungere a `crates/firstpass-proxy/Cargo.toml`:
  ```toml
  sha2 = "0.10"
  hex  = "0.4"
  ```

### 2.2 Creare `crates/firstpass-proxy/src/reflexion.rs`

Implementare in ordine:

- [ ] Struct `ReflexionRun` (output del loop).
- [ ] Struct `ReflexionCtx<'a>` (input/dipendenze del loop).
- [ ] Costante `DEFAULT_MENTOR_SYSTEM_PROMPT`.
- [ ] `fn hash_correction_sha256(correction: &str) -> String` — usa `sha2`, mai `DefaultHasher`.
- [ ] `fn normalized_edit_distance(a: &str, b: &str) -> f64` — Levenshtein normalizzato, O(n·m) time, O(n) space.
- [ ] `fn build_mentor_user_message(base_req: &ModelRequest, resp: &ModelResponse, failure_reasons: &[&str]) -> String` — sempre da `base_request`, troncato a 1000 chars.
- [ ] `fn parse_mentor_correction(raw: &str) -> Option<String>` — estrae `correction` dal JSON del mentor.
- [ ] `fn inject_correction(req: &mut ModelRequest, correction: &str, as_user_turn: bool)` — **ricostruisce sempre dal base**, non accumula.
- [ ] `fn make_abstain_attempt(...)` — helper per attempt di errore.
- [ ] `pub async fn run_reflexion_loop(ctx: &ReflexionCtx<'_>) -> ReflexionRun`:
  - `loop_start = Instant::now()` prima del `for` (per latency cap §10.9).
  - Check `max_latency_ms` all'inizio di ogni ciclo.
  - Rebuild `current_request` da `base_request` ad ogni ciclo (context isolation §10.1).
  - Convergence check dopo ogni output del 30B (§10.5).
  - Gate evaluation con health registry.
  - Se gate passa: popola `served_rung`, `break`.
  - Se gate fallisce e cicli rimasti: chiama mentor, aggiorna `latest_correction` (solo l'ultima, §10.1).
  - Hash SHA-256 della correction (§10.2).
  - Mentor input sempre da `base_request` (§10.3).

### 2.3 Test in `reflexion.rs` (tutti i 9 + nuovi)
- [ ] `parse_mentor_correction_valid_json`
- [ ] `parse_mentor_correction_with_preamble`
- [ ] `parse_mentor_correction_invalid_returns_none`
- [ ] `inject_as_system_prefix_prepends`
- [ ] `inject_as_user_turn_appends_message`
- [ ] `hash_correction_sha256_is_stable`
- [ ] `normalized_edit_distance_identical_strings`
- [ ] `normalized_edit_distance_completely_different`
- [ ] `normalized_edit_distance_partial_change`
- [ ] `normalized_edit_distance_empty_strings`
- [ ] `context_isolation_only_latest_correction_in_request`
- [ ] **NUOVO** `latency_cap_breaks_loop_immediately` — con `max_latency_ms = 0`, il loop esegue al massimo un ciclo.

### 2.4 Registrare il modulo
- [ ] Aggiungere `pub mod reflexion;` a `crates/firstpass-proxy/src/lib.rs`.

### 2.5 Verifica Fase 2
```bash
cargo test -p firstpass-proxy -- reflexion
cargo clippy -p firstpass-proxy --all-targets -- -D warnings
```
- [ ] Tutti i 12 test di `reflexion.rs` passano.

---

## Phase 3 — Self-verification gate (`crates/firstpass-proxy/src/gate.rs`)

*Obiettivo: aggiungere il tipo di gate `self_verify` basato su AutoMix/Self-REF (§10.8 del design).*

### 3.1 `config.rs` — nuovo `kind` per `GateDef`
- [ ] Aggiungere `SelfVerify { pass_when: String, prompt: Option<String> }` come variante di `GateKind` (o equivalente enum esistente).

### 3.2 `gate.rs` — implementare `SelfVerifyGate`
- [ ] Struct `SelfVerifyGate { id: String, prompt: String, pass_when: String }` che implementa il trait `Gate`.
- [ ] `evaluate(&self, req: &ModelRequest, resp: &ModelResponse) -> GateResult`:
  - Costruisce una seconda request al **modello dell'executor** (presa da `req.model`) con `max_tokens = 15`.
  - Il prompt di sistema: `"Answer only with a single word: high, medium, or low."`.
  - Il messaggio utente: `self.prompt` (default: `"Rate your confidence in the above response."`).
  - Parsea la risposta: se contiene `self.pass_when` (case-insensitive) → `Verdict::Pass`, altrimenti `Verdict::Fail`.
- [ ] Costruire `SelfVerifyGate` dal `GateDef` nel factory esistente.

### 3.3 Test per `SelfVerifyGate`
- [ ] `self_verify_passes_on_high` — mock executor risponde `"high"` → `Verdict::Pass`.
- [ ] `self_verify_fails_on_low` — mock executor risponde `"low"` → `Verdict::Fail`.
- [ ] `self_verify_fails_on_medium` quando `pass_when = "high"`.

### 3.4 Verifica Fase 3
```bash
cargo test -p firstpass-proxy -- gate
cargo clippy -p firstpass-proxy --all-targets -- -D warnings
```

---

## Phase 4 — Router & proxy integration (`router.rs`, `proxy.rs`)

*Obiettivo: collegare `reflexion.rs` al ciclo di enforcement principale.*

### 4.1 `router.rs` — `EnforceCtx`
- [ ] Aggiungere `pub reflexion: Option<&'a ReflexionConfig>` a `EnforceCtx`.

### 4.2 `router.rs` — hook in `route_enforce`

Aggiungere **prima** della chiamata a `run_ladder`:

- [ ] Se `ctx.reflexion.is_some()`: costruire `ReflexionCtx`, chiamare `run_reflexion_loop`.
- [ ] **Se `served_rung.is_some()`**: costruire `Trace` con tutti i campi reflexion, chiamare `trace.recompute_savings()`, `return (outcome, trace)` — **short-circuit, salta `run_ladder`**.
- [ ] **Se `served_rung.is_none()`**: applicare `on_reflexion_exhausted`:
  - `Error` → `return (EngineOutcome::Failed(...), build_failed_trace(...))`.
  - `ServeBestAttempt` → continuare con `run_ladder`, prepending `run.attempts` ai ladder attempts.
- [ ] Impostare `reflexion_latency_capped = Some(true)` nella trace se la latency cap è scattata.
- [ ] Dopo il ladder (fallback path): patchare `reflexion_cycles` e `mentor_cost_usd` sulla trace finale.

### 4.3 `router.rs` — bandit observe call (§10.4)
- [ ] Dopo `run_reflexion_loop`, chiamare `bandit.observe_with_cycles(&ctx_bucket, executor_rung, verdict, run.cycles_completed)` invece di `observe`.

### 4.4 `proxy.rs` — wiring
- [ ] Passare `reflexion: route.reflexion.as_ref()` nella costruzione di `EnforceCtx`.
- [ ] Estrarre header `X-Firstpass-Subagent` e popolare `features.subagent_name` prima del routing.

### 4.5 Verifica Fase 4
```bash
cargo check -p firstpass-proxy
cargo test -p firstpass-proxy
cargo clippy -p firstpass-proxy --all-targets -- -D warnings
```
- [ ] Test di integrazione con mock provider: loop completo executor→mentor→executor, `served_rung = Some(0)`.
- [ ] Test latency cap: `max_latency_ms = 1` → `reflexion_latency_capped = Some(true)`.
- [ ] Test exhausted `"error"`: `EngineOutcome::Failed`, nessun `served_rung`.

---

## Phase 5 — Bandit improvements (`bandit.rs`)

*Obiettivo: rendere il bandit consapevole dei cicli di reflexion e del subagent name.*

### 5.1 `ContextBucket`
- [ ] Aggiungere `subagent_name: Option<String>` alla struct `ContextBucket`.
- [ ] Aggiornare `ContextBucket::from_features` per popolare `subagent_name` da `features.subagent_name`.
- [ ] Verificare che `subagent_name: None` produca lo stesso hash/bucket di prima (backward-compat).

### 5.2 `observe_with_cycles`
- [ ] Implementare `pub fn observe_with_cycles(&mut self, ctx: &ContextBucket, rung: u32, verdict: Verdict, reflexion_cycles: u32)` con il discount `1 / (1 + cycles)`.
- [ ] Aggiungere test `assisted_pass_is_discounted_vs_unassisted_pass`.

### 5.3 Verifica Fase 5
```bash
cargo test -p firstpass-proxy -- bandit
cargo clippy -p firstpass-proxy --all-targets -- -D warnings
```
- [ ] Tutti i test bandit esistenti passano.
- [ ] Il nuovo test di discount passa.

---

## Phase 6 — Verifica finale del workspace

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

Checklist finale del design doc (§7 Acceptance Criteria):
- [ ] 1. 12 test `reflexion.rs` passano.
- [ ] 2. Backward-compat: trace senza campi reflexion serializza identicamente.
- [ ] 3. Hash chain: verifica su chain mista reflexion/non-reflexion.
- [ ] 4. Config con/senza `[route.reflexion]` parsano; `convergence_threshold = 1.5` fallisce.
- [ ] 5. Loop corretto: executor-fail → mentor → executor-pass.
- [ ] 6. Context isolation: request del ciclo 2 contiene solo correction-2.
- [ ] 7. Convergenza: output identici → `converged = true`.
- [ ] 8. SHA-256 stabile tra run.
- [ ] 9. Latency cap: `max_latency_ms = 1` → `reflexion_latency_capped = Some(true)`.
- [ ] 10. Exhausted `"error"` → `EngineOutcome::Failed`.
- [ ] 11. Self-verify gate: parse + mock test pass/fail.
- [ ] 12. Bandit discount: `cycles=2` < `cycles=0` per stesso numero di osservazioni.

---

## Ordine di commit raccomandato

```
1. feat(core): add ReflexionConfig, ReflexionExhaustedPolicy, reflexion fields on Config/Route
2. feat(core): add reflexion trace fields on Attempt and FinalOutcome
3. feat(core): add subagent_name to Features
4. feat(proxy): add sha2/hex deps, implement reflexion.rs with all 12 unit tests
5. feat(proxy): implement SelfVerifyGate (gate.rs + config.rs kind dispatch)
6. feat(proxy): wire ReflexionEngine into route_enforce (router.rs + proxy.rs)
7. feat(proxy): add observe_with_cycles and subagent_name to bandit (bandit.rs)
8. test: verify full workspace green
```

> **Non mixare** il commit 6 (router) con il commit 7 (bandit) — sono indipendenti e più facili da revertire separati.
