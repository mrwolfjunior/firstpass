# SDD ledger — plan: docs/superpowers/plans/2026-09-20-reflexion-loop-implementation.md

**Branch:** `feat/reflexion-loop`  
**Base commit:** `8f9ac1379dcb0652edf5e5862dcb2e828184b00e`  
**Spec:** `docs/superpowers/specs/2026-09-19-reflexion-loop-local-design.md`  

---

## Pre-flight conflict scan

| Task pair | Shared surface | Finding |
|---|---|---|
| Phase 1 → Phase 2 | `ReflexionConfig` struct | Phase 2 imports `firstpass_core::config::ReflexionConfig` — must complete after Phase 1 |
| Phase 1 → Phase 3 | `GateDef` enum / `GateKind` | Phase 3 adds `SelfVerify` variant — must complete after Phase 1 config changes |
| Phase 1 → Phase 5 | `Features.subagent_name` | Phase 5 reads `features.subagent_name` in `ContextBucket::from_features` — must complete after Phase 1 |
| Phase 2 → Phase 4 | `reflexion.rs` public API (`ReflexionCtx`, `run_reflexion_loop`) | Phase 4 calls these directly — must complete after Phase 2 |
| Phase 3 → Phase 4 | `SelfVerifyGate` constructed from `GateDef` | Phase 4 wires the gate factory — must complete after Phase 3 |
| Phase 1 self | `deny_unknown_fields` on `Config` and `Route` | New fields must be declared explicitly — verified in spec §2 |
| Phase 1 self | `skip_serializing_if = "Option::is_none"` on all new trace fields | Hash chain backward-compat — verified in spec §3 |

**Ruling:** Execution order must be strictly Phase 1 → Phase 2 → Phase 3 → Phase 4 → Phase 5 → Phase 6. No parallelism between phases.

---

## Progress

- [x] Phase 1 — Core schema & config
- [ ] Phase 2 — Reflexion engine (reflexion.rs)
- [ ] Phase 3 — Self-verification gate
- [ ] Phase 4 — Router & proxy integration
- [ ] Phase 5 — Bandit improvements
- [ ] Phase 6 — Final verification
