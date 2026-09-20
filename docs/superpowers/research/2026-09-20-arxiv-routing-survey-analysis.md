# Analisi: "Dynamic Model Routing and Cascading for Efficient LLM Inference" → Firstpass

**Paper:** arxiv.org/html/2603.04445v2  
**Data analisi:** 2026-09-20  

---

## Sintesi del paper

Il paper è una survey sistematica su tutti i paradigmi di routing/cascading multi-LLM. Classifica i sistemi lungo 3 dimensioni:
- **Quando** la decisione viene presa (pre-generation / post-generation / multi-stage)
- **Cosa** usa come segnale (query / model metadata / response / feedback)
- **Come** computa la decisione (heuristic / supervised / bandit / RL policy)

---

## Mapping: paradigmi del paper → stato attuale di firstpass

| Paradigma paper | firstpass oggi | Gap |
|---|---|---|
| **Difficulty-aware routing** | `features.task_kind` + `prompt_token_bucket` nel bandit | Parziale — nessun classifier esterno |
| **Human preference-aligned** | Non presente | Assente |
| **Clustering-based** | Non presente | Assente |
| **RL routing (bandit)** | UCB1 + Thompson sampling `StartRungBandit` | ✅ già solido |
| **Uncertainty-based** | Conformal calibration (`conformal.rs`) | ✅ già solido |
| **Cascading** | Ladder escalation in `router.rs` + Reflexion Loop (in design) | ✅ architettura corretta |

---

## Concetti direttamente ereditabili (priorità alta)

### 1. Self-verification come gate del 30B (da AutoMix §7, Self-REF §7)

**Cosa dice il paper:** AutoMix usa il modello piccolo stesso per stimare la qualità del proprio output prima di scalare al modello grande. Questo "self-verification score" è il trigger di escalation — non un gate esterno. Self-REF fa lo stesso ma con fine-tuning di token speciali `<CN>` (confident) e `<UN>` (unconfident).

**Rilevanza per firstpass:** Attualmente il gate è sempre esterno (subprocess / judge / schema). Il paper dimostra che il 30B può stimare la propria confidenza autonomamente, il che è molto più veloce (nessuna chiamata extra) e non richiede ground truth.

**Come adattarlo:**
```toml
# Nuovo tipo di gate in gate.rs: "self_verify"
[[gate]]
id      = "self-verify"
kind    = "self_verify"
prompt  = "Rate your confidence in the above response: high/medium/low. Answer only the rating."
threshold = "high"    # accetta solo se il modello risponde "high"
```

Il 30B genera la risposta, poi in una seconda chiamata (prefill = gratis) chiede a se stesso la confidenza. Se `low` o `medium`, trigger del mentor. **Questo elimina il bisogno di gate external sul 30B per molti casi.**

> **Costo:** una seconda chiamata al 30B da ~10 token = < 0.1 secondi. Trascurabile.

---

### 2. Quality estimation come segnale di escalation variabile (da FrugalGPT §7, Cascade Routing §7)

**Cosa dice il paper:** FrugalGPT usa un quality estimator (DistilBERT) che assegna uno score continuo `[0,1]` alla risposta. La decisione di escalare non è binaria (pass/fail) ma usa una soglia configurabile. Cascade Routing identifica la **quality estimation** come il fattore critico per il successo del model selection.

**Rilevanza per firstpass:** Il sistema di conformal calibration in `conformal.rs` fa esattamente questo — calibra una soglia continua. Ma questa threshold è globale per rung. Il paper suggerisce che la soglia dovrebbe essere **per-contesto** (task_kind × difficulty).

**Come adattarlo:**
- Il `serve_threshold` in `EnforceCtx` è già un `Option<f64>` — è pronto per essere per-contesto.
- Estendere `AdaptiveConformal` in `conformal.rs` per avere un threshold separato per `ContextBucket` (non solo per rung).

---

### 3. Response-level signal + online adaptation (gap identificato in §10)

**Cosa dice il paper (§10 — gap esplicito):** *"No current method simultaneously pairs response-level signals with online adaptation, as uncertainty-based approaches and cascades exploit response signals but remain static once deployed, while bandit-based methods adapt online but operate on query-level signals alone."*

**Questo è esattamente il gap che il Reflexion Loop di firstpass colma.** Il 80B mentore legge la risposta (response-level signal) e il bandit aggiorna le sue stime (online adaptation). Firstpass è nella frontiera di ricerca su questo punto.

**Nessuna modifica necessaria — è già nel design.**

---

### 4. LinUCB contestuale invece di UCB1 (da PILOT §5.2, GreenServ §5.2)

**Cosa dice il paper:** PILOT e MixLLM usano **LinUCB** (Linear UCB) invece di UCB1 classico. LinUCB sfrutta feature vettoriali della query (non solo il bucket coarso) per stimare il reward — funziona con contesti ad alta dimensionalità e converge più velocemente.

**Rilevanza per firstpass:** `StartRungBandit` usa UCB1 su bucket `(task_kind, prompt_bucket_coarse)`. Con il `subagent_name` aggiunto (§10.6 del design), il bucket diventa un vettore a 3 dimensioni. Se si aggiungono più feature (tipo di task, lunghezza output, complessità sintattica), UCB1 non scala bene — LinUCB sì.

**Come adattarlo (miglioramento futuro, non immediato):**
- Sostituire `HashMap<ContextBucket, HashMap<u32, ArmCounts>>` con una regressione lineare ridge per stimare `E[pass | feature_vector, rung]`.
- La feature vector esiste già (`Features` struct in `features.rs`) — è solo una questione di usarla come input del bandit invece di coarsenizzarla in bucket.

> **Quando farlo:** solo dopo aver verificato che il bucket coarso non sia già sufficiente. Misurare la performance del bandit attuale sul traffico reale prima.

---

### 5. Discounting per non-stazionarietà (da TI-UCB §5.2, MixLLM §5.2)

**Cosa dice il paper:** TI-UCB e il Thompson discounted (già in `bandit.rs`) affrontano il problema che i modelli cambiano nel tempo (nuovi checkpoint, nuovi fine-tuning). Il discount factor `δ < 1` applica decadimento moltiplicativo alle osservazioni vecchie.

**Rilevanza:** Il discount è già implementato in `bandit.rs` (`discount: f64`). Il paper conferma che questa è la soluzione giusta. **Nessuna modifica necessaria** — è già fatto.

---

### 6. Firewall routing: bloccare query "unsolvable" (da nota §7)

**Cosa dice il paper:** Il Firewall Routing identifica query che nessun modello nel pool può risolvere e le blocca preventivamente, evitando di chiamare il modello grande inutilmente.

**Rilevanza:** Nel contesto locale 30B+80B, questo si traduce in: se il gate fallisce per N cicli su una query e la convergence detection rileva nessun progresso, la query è probabilmente "unsolvable" per questo pool. Invece di escalare all'80B nella ladder, restituire un errore esplicito all'harness.

**Come adattarlo:**
- Aggiungere `on_reflexion_exhausted: ServePolicy` a `ReflexionConfig`: `"serve_best_attempt"` (default) oppure `"error"` (restituisce errore esplicito se converged=true).
- Già quasi possibile con `on_exhausted` in `[budget]` — ma si può rendere più granulare a livello reflexion.

---

### 7. Subtask decomposition per il 80B orchestratore (da R2-Reasoner §5.1)

**Cosa dice il paper:** R2-Reasoner scompone task complessi in subtask semplici, assegnando ciascuno al modello più adatto. Un Task Decomposer (modello grande) + Subtask Allocator (bandit).

**Rilevanza:** Nel tuo setup, l'80B è già l'orchestratore. Il paper formalizza questa architettura e dimostra un 84% di riduzione costi API. In firstpass, il 80B potrebbe non solo **correggere** il 30B (Reflexion Loop) ma anche **decomporre** il task prima di inviarlo al 30B.

**Come adattarlo (futuro):**
- Aggiungere un'opzione `decompose: bool` al `ReflexionConfig`: se true, la prima chiamata all'80B non è una correzione ma una decomposizione del task in subtask ordinati.
- Il 30B esegue ciascun subtask in sequenza.
- Utile per task molto lunghi (file grandi, codebase complesse).

---

## Concetti NON ereditabili (e perché)

| Concetto | Motivo |
|---|---|
| **Clustering-based routing** (UniRoute, Avengers-Pro) | Richiede embedding di query + K-means offline. Il contesto locale non ha abbastanza varietà di modelli per giustificarlo. Il bucket coarso del bandit è sufficiente. |
| **Human preference routing** (RouteLLM, Chatbot Arena) | Richiede dataset di confronti umani. In contesto locale vibe-coding, non si hanno dati comparativi. |
| **Matrix factorization / EmbedLLM** | Richiede molti modelli nel pool. Con solo 2 modelli (30B + 80B) non c'è nulla da fattorizzare. |
| **Multimodal routing** (ReLope, MMR-Bench) | Non rilevante — entrambi i modelli sono text-only nel setup corrente. |
| **PPO/GRPO policy optimization** (Router-R1, R2-Reasoner) | Richiede training con RL. Fuori scope per un proxy locale senza supervision esterna. |

---

## Riepilogo: cosa aggiungere al design document

| Idea | Priorità | Complessità | Dove nel design |
|---|---|---|---|
| **Self-verification gate** (§1 sopra) | Alta | Media | Nuovo tipo di gate in `gate.rs` + config `[[gate]]` |
| **Per-context conformal threshold** (§2) | Media | Media | Estensione `conformal.rs` + `AdaptiveConformal` |
| **`on_reflexion_exhausted`** (§6) | Alta | Bassa | Campo aggiuntivo in `ReflexionConfig` |
| **LinUCB** (§4) | Bassa | Alta | Sostituzione `bandit.rs` — solo dopo misure reali |
| **Task decomposition** (§7) | Bassa | Alta | Futuro — dopo che il loop base funziona |

---

## Insight chiave dal paper applicato a firstpass

Il paper identifica in §10 tre gap strutturali nella letteratura corrente:

1. **Nessun sistema combina response-level signals con online adaptation** → firstpass li combina (mentor legge la risposta, bandit aggiorna online). **Firstpass è ahead of the state of the art su questo punto.**

2. **RL è poco usato nel cascading** → Il bandit UCB1/Thompson di firstpass è una forma leggera di questo. Il discounted Thompson (già implementato) è esattamente ciò che serve.

3. **Pochi sistemi trattano quality, cost, latency come obiettivi multi-dimensionali** → firstpass ha `[budget]` per cost, ma non ha un obiettivo di latency esplicito. Con i modelli locali, la latency è la metrica principale. Aggiungere un `max_latency_ms` a `ReflexionConfig` che stoppa il loop se la latenza cumulata supera una soglia sarebbe un differenziatore rispetto alla letteratura.
