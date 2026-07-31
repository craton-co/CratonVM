# Native ANTLR intrinsics lose object roots under the moving young collector

**Status:** FIXED 2026-07-31 — the defect *class* is gone, not just the sites
that were found. `native-builtins/src/antlr_intrinsics.rs` no longer contains a
single `pin_native_root` / `read_native_pin` / `unpin_native_roots` call; every
reference held across an allocating call is a `NativeHandle` owned by a
`NativeHandleScope`, and a guard test keeps the raw API out.

**Witness:** `org.hibernate.orm.test.hql.ASTParserLoadingTest`, most easily
under `--nojit`. It was **not** `--nojit`-only: under enough load (a 6-shard
corpus run in parallel) it also reached JIT mode, there as
`NoSuchMethodError: java.lang.Object.getText(Interval)` — a receiver whose class
word was read out of moved memory.

## Symptom

Valid HQL was rejected at parse time, nondeterministically — a different subset
of tests failed on each run, and once a run started failing it usually failed
several more times. Every failure had the same shape: a comparison operator
directly after a function call or a parenthesized expression.

```
SyntaxException: At 1:43 and token '>', no viable alternative at input
  'from Animal an where sqrt(an.bodyWeight)/2 *> 10'
SyntaxException: At 1:38 and token '=', mismatched input '=' expecting I
  'from Human h where -(h.intValue - 100)=74'
```

(The `*` is ANTLR's error-position marker, not corrupted input — the bracketed
original query in the exception is intact. An earlier investigation read this as
token-stream corruption; it is not.)

## What it was NOT

Two measurements ruled out the explanation that the 2026-07-29 investigation
adopted (and acted on, by deleting `dev`'s trivial-accessor fast path):

1. `CRATONVM_TRIVIAL_GETTER_VERIFY=1` cross-checks every stackless-accessor hit
   against `resolve_field_ref_loader_aware`, the resolver the real `getfield`
   opcode uses. A full 106-test run reported **zero** divergences in field
   index, descriptor byte, reference-ness, volatility, or owning class — and
   still mis-parsed.
2. The same binary with the fast path still enabled and
   `CRATONVM_NO_MOVING_YOUNG=1` passed **106/106**.

The fast path only changes allocation and safepoint timing. It was not the bug.

## Root cause

`antlr_intrinsics.rs` reimplements ANTLR's `ParserATNSimulator`
closure/reach/merge machinery. It was written throughout on raw `ObjectRef`
locals — including `Vec<ObjectRef>` snapshots of whole config sets and parent
arrays — held across calls that allocate and therefore can run a moving young
collection. A relocated object leaves those locals pointing at dead memory, and
the result is silently linked into the parser's own graph.

The damage is amplified by ANTLR's own design: a wrong config or parent poisons
the closure/reach set, whose result is **memoized as a DFA edge**. One hit
therefore breaks every later prediction that reuses that edge — which is why a
single mis-timed collection produced a cluster of failures and why the affected
grammar path (`<expression> <comparison-op>`) stayed broken for the rest of the
process.

`CRATONVM_MOVING_YOUNG_VERIFY=1` reported `forwarded_heap_refs_remaining
young=0 old=0` on every evacuation, which is consistent: the *heap* graph is
fully remapped. The stale references lived in Rust locals the verifier does not
scan. `CRATONVM_DBG_BLOCKGC=1` also stayed silent, because its canary only
recognises addresses still present in the current cycle's pointer map.

## The fix (2026-07-31)

Three earlier passes (2026-07-22, and two on 2026-07-30) each converted "a few
more" sites by hand and each left the file's default style intact. This pass
changed the style instead, which is what the previous revision of this document
recommended:

* **Every** raw-pin call site is gone — all 344 of them. The module now roots
  through `NativeHandleScope` / `NativeHandle` (`native-api/src/registry.rs`,
  "rooted handle scope"). A handle is an opaque `u32` slot id rather than a heap
  address, so the pre-collection reference cannot be read back by mistake, and
  the scope closes on normal return, on `?`, and on unwind.
* That also removes the recurring **truncating-unpin hazard**:
  `unpin_native_roots(base)` truncates the pin stack, so releasing an early pin
  silently dropped every root taken after it. That mistake was made twice in
  this file alone. Nested `NativeHandleScope`s release only their own handles.
* `antlr_atn_config_set_config_vec` is **deleted**. Config sets are walked by
  index out of the rooted set via `antlr_config_set_at`. A `Vec<ObjectRef>`
  snapshot is stale the moment the loop body allocates, and rooting the whole
  snapshot to compensate makes every config of every set a GC root for the
  entire closure walk — on exactly the allocation-heavy path that provokes the
  bug. Index re-reads are correct *and* O(1) in roots. The hot walks
  (`compute_reach_set`, `closure_`, `removeAllConfigsNotInRuleStopState`) also
  nest a scope per entry, so per-entry handles are released as the walk
  advances instead of accumulating one per config.
* A source-level guard test,
  `antlr_prediction_context_tests::raw_pin_api_is_not_used_in_this_module`,
  fails the build if any of the three functions reappears. It is a source check
  rather than a behavioural one because the defect only manifests when a
  collection lands inside a specific window — which is precisely what no unit
  test can schedule.

### Defects found while converting

Each of these is a reference read *after* a call that can collect. All were
live on `dev` at 2026-07-31 (`a31a8a93f`), i.e. they survived both 07-30 passes.

| Site | Defect |
|---|---|
| `native_antlr_array_prediction_context_init_singleton` | pinned the parent and **discarded the handle**, then stored the pre-pin reference into the new parents array — the exact defect the 07-30 table claims to have fixed |
| `antlr_create_singleton_context`, `antlr_create_array_context`, `antlr_empty_instance`, `antlr_new_linked_hash_map`, `antlr_alloc_common_token` | returned the object allocated *before* its initializer ran |
| `native_antlr_parser_can_drop_loop_entry_edge` | completely unrooted, while comparing `state` / `blockEndState` **by identity** against states read back through `List.get` |
| `native_antlr_default_error_strategy_report_input_mismatch` | six references held across seven consecutive `invoke_virtual` calls |
| `antlr_interval_set_contains`, `antlr_config_list_find`, `antlr_atn_config_set_config_list_hash`, `antlr_atn_config_set_configs_equal`, `antlr_semantic_operands_hash` | iterated a backing array across a per-element comparison/hash that runs Java |
| `antlr_atn_configs_equal`, `antlr_atn_config_set_equals`, `antlr_lexer_atn_config_hash`, `antlr_lexer_atn_configs_equal`, `antlr_atn_config_set_hash` | read their operands again *after* the semantic-context comparison (`&&` evaluates left to right, so the later terms are the stale ones) |
| `antlr_object_hash` | the identity-hash fallback read the receiver after `hashCode()` |
| the SemanticContext AND/OR combine path (`push_unique` / `collect_operands` / `filter_precedence` / `sort_operands` / `context_combine`) | carried a whole `Vec<ObjectRef>` of operands across equality, hashing and sorting; now a `Vec<NativeHandle>` threaded through one scope |
| `native_antlr_atn_config_init`, `native_antlr_lexer_atn_config_init` | wrote `this`, `state` and `context` after `SemanticContext$Empty`'s `<clinit>` |
| `native_antlr_common_token_get_text`, `antlr_copy_text_if_requested` | used the token / input stream after `Interval.of` |
| `antlr_atn_config_set_add_impl` | `antlr_config_list_find(...)?.unwrap_or(config)` fell back to the *pre-call* `config` |

## Verification, and what it does and does not prove

`dev` at `a31a8a93f` versus the converted binary, both built and run on this
host (`cratonvm-antlrbase-20260731.exe` / `cratonvm-antlrfix2-20260731.exe`):

| Arm | dev baseline | converted |
|---|---|---|
| `ASTParserLoadingTest` ×6, `--nojit` | 106/106 | 106/106 |
| `ASTParserLoadingTest` ×8, JIT, interleaved A/B | see "the `?1` failure" below | see below |
| `HqlParseStress` 250×14 parses, {jit,nojit} × {default, `GC_STRESS=4M`} | 0 misparsed | 0 misparsed |
| `HqlParseStress`, jit × `GC_STRESS=1M` | timeout at 1800s | timeout at 1800s |
| Hibernate `others` corpus (141 classes), `--nojit`, 4 shards | reference | **byte-identical per-class status** |
| `cargo build --release --workspace` | clean | clean |
| `cargo test -p cratonvm-native-builtins` | 3153 pass | 3153 pass (incl. the new guard) |

The corpus row is the strongest no-regression evidence: `diff` of
`(class, status)` over all 141 classes is empty
(`CRASH=1 ABORTED=5 HANG=7 FAIL=37 NOTESTS=91` on both). `ASTParserLoadingTest`
itself reports HANG in that arm on **both** binaries — it needs 350-650s and the
runner's flat cap is 300s; the standalone witness script runs it with a 900s cap,
where it passes.

**Be honest about what this shows.** Two limitations, both real:

1. **The baseline was already green** on both witnesses on this host — the 07-30
   pass had closed the paths they exercise. So the run counts demonstrate *no
   regression*, not *a repaired failure*.
2. **No arm is known to have relocated anything.** `CRATONVM_GC_STRESS` does
   multiply minor collections (measured on `HqlParseStress`: 3 → 21 → 595 over
   the same workload as the threshold tightens), but
   `CRATONVM_MOVING_YOUNG_VERIFY=1` emitted **zero** `[moving-young-verify]`
   lines in every configuration tried — default and `CRATONVM_DBG_FORCE_MOVING=1`,
   1500m and 256m heaps, JIT and `--nojit`. So there is no positive evidence
   that the moving path ran during any of these runs, and "0 misparsed across
   the GC-stress matrix" must not be read as "survived N evacuations".
   Note also that `[GC] moving_young: cycles=N` is **not** the counter to use
   here: `record_moving_young_cycle` only fires when the moving collection
   happens *while a JIT frame is live* (`gen_heap.rs`), so `cycles=0` under
   `--nojit` is expected and says nothing. Anyone extending this work should
   first establish a configuration in which the verifier actually reports, and
   should treat the GC-stress lever as unproven until then — see
   [[reference_inert_lever_is_not_an_elimination]].

The value of this change therefore rests on the other half, which does not
depend on provoking the race: the defect class can no longer be written, and the
twelve concrete unrooted sites in the table above — every one of them a live
"read after a collection point" on `dev` at `a31a8a93f` — are fixed. A clean
corpus was never going to be proof that none were left; making the pattern
unrepresentable is.

### The `?1` dropped-parameter failure

One of six JIT witness runs on the converted binary failed
`testComponentNullnessChecks` with
`No parameter labelled '?1' in query with ordinal parameters []` — the query
`from Human where ?1 is null` parsed, but its parameter never reached
`ParameterMetadataImpl`. That is **not** this document's symptom: no
`SyntaxException` appeared in any of the twelve JIT/`--nojit` witness runs, and
this shape is a silently missing production rather than a rejected parse. It is
tracked separately in
`docs/known-issues/hibernate/hql-ordinal-parameter-dropped-under-jit-20260731.md`;
`HqlParseStress` now carries parameter queries and a tree-text assertion so the
fast probe can catch that shape too.

New fixtures, tracked next to the suite runner:

* `apps/hib-suite-runner/run-astparser-witness.sh` — repeat the witness class N
  times against one binary and tally misparses. A single green run proves
  nothing for a nondeterministic defect; only a run count does.
* `apps/hib-suite-runner/HqlParseStress.java` +
  `apps/hib-suite-runner/run-hql-stress.sh` — a ~3-minute inner-loop gate that
  drives Hibernate's real HQL ANTLR parser over the exact grammar path this
  defect breaks, thousands of times per process, across a JIT × GC-stress
  matrix. `ASTParserLoadingTest` takes ~5 minutes per run and reproduces only
  intermittently, which made it useless for iteration. Compile the probe from
  the runner directory with
  `javac @cp-javac.args -d . HqlParseStress.java` (the argfile is derived from
  `common.args` with backslashes turned into forward slashes — javac treats a
  backslash inside a quoted argfile string as an escape).

## History

This continues a 2026-07-22 pass over the same file
(`hibernate-atnstate-transitions-npe-intermittent-hql-parse-20260721-FIXED.md`),
which fixed the closure walk's `ATNState.transitions` instance of the identical
pattern, and two independent 2026-07-30 passes (one recorded in the previous
revision of this document, one merged to `dev` while that work was in flight).
Four passes; the first three each found "a few more" by hand and none had a
completion signal. The fourth changed the representation.

## Relationship to HIB-BYTEBUDDY

None, beyond being found during its verification. This defect predates the
ByteBuddy work and is independent of the `net/bytebuddy/` JIT ban. The ByteBuddy
closure is recorded in
`docs/internal/fixed-suite-bugs/hibernate/hib-bytebuddy-20260730-FIXED.md`;
`ASTParserLoadingTest` was the single non-PASS in its 302-class corpus for
several intermediate binaries, and it was this issue.
