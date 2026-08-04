# COV-05 — RETIRED 2026-08-04. Both halves shipped; checkcast did not need cov-07

Was `docs/known-issues/c2/cov-05-checkcast-and-instanceof.md`. Owned the
`!scan.typecheck_ops.is_empty()` conjunct of `ir_compatible`, the `0xc0`/`0xc1`
arms it would admit, and their lowering.

The brief staged this as two increments — `instanceof` first (no exception
edge), then `checkcast` only if it turned out independent of `cov-07`'s
`athrow` question, "sequence them rather than discovering it in a merge"
otherwise. Both shipped in one session: `checkcast`'s throw turned out to be
the same generic sentinel-drain protocol `Op::ConstClass`'s resolution
failure already uses, not `athrow`'s bci-baked exception-table machinery.

## What the lane shipped

| | |
|---|---|
| `Op::InstanceOf` | `[ctrl, mem, obj]` → `Int`. Calls `helpers.instanceof_check` — the identical helper, identical ABI, the single-pass 0xc1 arm calls. Never throws. |
| `Op::CheckCast` | `[ctrl, mem, obj]` → `Ref`. Calls `helpers.checkcast` — same pattern. A definitive refusal stashes a `ClassCastException` and returns the `i64::MIN` sentinel, drained through `Lowerer::emit_call_return_check` — the SAME machinery `Op::Call`'s callee-exception path and `Op::ConstClass`'s resolution failure already use. |
| admission gate | Both sites admitted only when the target class resolves `JitNewSite::Resolved` (already loaded) via the SAME `cp_new_resolver` `new`/`anewarray` use. `Deferred`/`None` (not yet loaded, or unresolvable) omits the pc from `checkcast_info`/`instanceof_info`, bailing that site — and so the method — to single-pass. |
| `checkcast` / `athrow` independence | Established before writing the lowering, per the brief's instruction — see below. |
| tests | 15 in `jit/tests/ir_vs_singlepass.rs`: exact-class/subclass/interface/miss/null × 2 ops, not-yet-loaded-refuses-IR × 2 ops, and one method mixing both opcodes at two pcs sharing one cp index. |

## Why `checkcast` did not need to wait for `cov-07`

The brief's hazard was real but pointed at the wrong mechanism. `athrow`'s
single-pass lowering bakes a bci as an immediate for
`route_jit_exception_through_method`, which range-tests it against the
compiling method's OWN exception table — that is genuinely unbuilt in the IR
tier, and is what `cov-07` has to solve if it proceeds at all.

`checkcast`'s failure path is a different, much older mechanism. Reading
`jit_checkcast` (`vm/src/jit/helpers.rs`): on a definitive refusal it calls
`create_exception_object`, `set_jit_pending_exception`, and returns
`i64::MIN` — exactly what `jit_ldc_class_cp` (`Op::ConstClass`'s helper) does
on a resolution failure, and exactly what `jit_getstatic` does when
`<clinit>` throws. The single-pass backend's own comment at the checkcast
call site says it plainly: *"the codegen's post-helper check
(`emit_post_invoke_exception_check`) routes it through the standard
pending-exception drain."* Not the exception table. Not a bci. A sentinel
check and an unconditional jump to a shared bail stub that runs the method
epilogue and returns — the IR tier's `Lowerer::emit_call_return_check` is the
exact same shape, already exercised by every `Op::Call` site.

The reason this is sound, not just similar-looking: neither this drain nor
`athrow`'s machinery ever dispatches into a LOCAL exception-table handler
inside the currently-compiled frame. A JIT frame that throws — from any
helper, any opcode — always unwinds the WHOLE compiled method and hands a
pending exception to the caller; local-handler dispatch happens only after
falling back to the interpreter with a **precise** resume frame. The
`ir_compatible` admission chain already refuses that precise-frame case
categorically (`precise_exception_frames`, RBC.6: "a handler reads a
non-parameter local"), independently of which bytecode instruction would
throw. Every method that reaches `Op::CheckCast`'s lowering has therefore
already passed that gate, for the same reason `Op::Call`, `Op::ConstClass`
and `Op::LoadStatic` already have. `athrow` is a harder problem for a
different reason — 89 events where the exception's SOURCE is the bytecode
itself, needing the bci-baked handler range test — not because compiled code
can throw at all.

Net: `checkcast` and `athrow` are independent lanes. `cov-07` still owns
`scan.has_athrow` and the question of whether admitting explicit `athrow`
sites is worth the exception-edge modelling it would need; nothing here
changes that lane's scope or its own recommendation to size it last.

## Design

`checkcast`'s and `instanceof`'s constant-pool entries are both plain
`CONSTANT_Class` references — the identical shape `new`/`anewarray` already
resolve via `cp_new_resolver` (`JitNewSite::Resolved{class_id,..}` = loaded,
`Deferred` = not yet, `None` = malformed). Reusing that resolver rather than
adding a new one meant **zero new `try_compile` parameters** and zero call-site
changes across `jit_bridge.rs`/`interpreter.rs`/the test harness — the
existing `(cp_new_resolver, cp_class_name_resolver)` pair, already threaded
everywhere, was enough.

`jit/src/x64/bytecode_compat.rs`'s scanner grew one new field,
`checkcast_ops: Vec<(usize, u16)>` — a subset of the pre-existing
`typecheck_ops` (`checkcast_ops ∪ instanceof_ops = typecheck_ops`), populated
only at 0xc0. `ir_compatible` gates on `checkcast_ops` alone now (not
`typecheck_ops`), and — as of the second half of this lane — does not gate on
it at all: neither opcode refuses the whole method any more. `lib.rs`'s
`try_compile_inner` splits `scan.typecheck_ops` per-pc into `checkcast_info`
and `instanceof_info` using `scan.checkcast_ops` as the discriminator, which
is what makes a method containing BOTH opcodes (previously impossible to admit
at all, since any `checkcast` refused the whole method) route each site to
the correct `Op` variant.

Both new ops carry `[ctrl, mem, obj]` and `MemAccess::Opaque` — the same
shape cov-01's `Op::ConstString`/`ConstClass`/`LoadStatic` established, which
is what makes them correct in the scheduler, alias model, DCE, and the EA
bridge with no per-node opinion (all three of `ir_optimize::memory_token_slot`
/ `ir_verify::is_memory_token_input` / `lib.rs::ea_memory_token_slot` already
delegate to the single `Op::memory_shape()` table — cov-01's own
consolidation paid for this lane before it started). What DID need explicit,
per-op registration, because Rust's exhaustiveness checking is the only thing
that catches an omission here: `ir_lower::op_defines_result_slot` +
`regalloc.rs`'s verbatim mirror, `ir_lower::declared_lowering`'s exhaustive
match (no wildcard, by design — a missed op fails the BUILD, not a test),
`ir_verify::expected_arity`'s exhaustive match (same property, and this one
was NOT mentioned by the file's own "three enumerations" doc comment — a
fourth, found only by grep, worth remembering for the next op added here),
and `regalloc.rs`'s two other predicates (`ir_op_is_safepoint`,
`ir_op_is_call`) which are supersets by design and needed both new ops added
for the same reason `Op::Call`/`Op::ConstString` are in them: the lowering
lands `MOV RAX,helper ; CALL RAX` and returns into the body, so a
caller-saved register live across it must not be assumed to survive.

`checkcast`'s failure path additionally sets `compiled.has_dispatch = true`
whenever any checkcast site is admitted — `jit_checkcast`'s exception
construction needs `jit_thread_mut()` (the JIT_THREAD TLS), same requirement
`jit_getstatic`'s `<clinit>` execution has, and the same
`jit-clinit-gap-has-dispatch` defect shape the single-pass backend's own
`emitted_checkcast_throw` flag exists to avoid.

## Verification

Both arms built `--release`, same host, same 38-test `ConditionalOnPropertyTests`
class, same env (`CRATONVM_REAL=net-sockets,aqs CRATONVM_THREADS=-default-watchdog
CRATONVM_JIT=rootsnap-cache`), `CRATONVM_DBG=ir-compiles`. Baseline is `dev`
at `36b5b16b7` (the commit this branch forked from, carrying `cov-01`–`cov-04`),
built the same day.

| | baseline (`dev`, pre-cov-05) | fixed |
|---|---:|---:|
| `SBRUNNER_RESULT` | `tests=38 failed=0 aborted=0` | `tests=38 failed=0 aborted=0` |
| compile requests | 1,231 | 1,432 |
| admitted to the optimizing pipeline | 633 | 817 |
| **bodies the optimizing backend produced** | **379** | **756** |
| `typecheck_ops`/`checkcast_ops` refusals | **169** | **0** |
| `anewarray_ops` refusals (`cov-06`'s conjunct) | 70 | 66 |
| `has_athrow` refusals (`cov-07`'s conjunct) | 52 | 46 |
| `indy_ops` refusals (unowned) | 44 | 38 |

**The typecheck refusal is gone — 169 → 0 — and bodies almost doubled, 379 →
756 (+99%).** All 38 real Spring Boot tests pass identically in both arms;
zero regressions. The three other refusal categories all fell too, which is
the same re-ranking effect `cov-01`–`cov-04` each produced on their
neighbours: a method that used to die on `typecheck_ops` before reaching the
builder can now run far enough to be counted (and sometimes admitted) against
one of the other three conjuncts instead of never being requested/measured at
that granularity. Re-survey before sizing `cov-06`/`cov-07` — this table
already is that re-survey for this one corpus, and both remaining conjuncts'
counts are lower than the original `ir-coverage-survey-20260803.md` numbers
they inherit, for the same reason.

The debug-binary run that preceded this table (same class, same env, no
`--release`) took 850s just to finish Spring context bootstrap under host
contention and never reached `SBRUNNER_RESULT` before its own timeout — it is
not in this table, but it independently confirmed zero `checkcast_ops`
mentions and zero crashes across 3,545 log lines / 775 admissions / 716
bodies of real Spring Boot bytecode before it was superseded by the faster
release run above. Worth recording: on a heavily loaded shared host, build
`--release` before attempting a full-suite timing verification — the debug
interpreter/JIT tax compounds with contention badly enough to make a 10-minute
budget insufficient for a single test class's context bootstrap alone.

Unit coverage, `jit/tests/ir_vs_singlepass.rs` (15 new tests, all passing
alongside the pre-existing 1,879 lib / 138 total integration tests):

* `ir_vs_singlepass_instanceof_{exact_class_hit,subclass_hit,interface_hit,miss,null}`
  and the `checkcast` siblings
  (`ir_vs_singlepass_checkcast_{exact_class_succeeds,subclass_succeeds,interface_succeeds,definitive_refusal_returns_sentinel,null_always_succeeds}`)
  — IR and single-pass compiled against the SAME stub helper, asserted equal
  to each other and to the expected answer. The stub mirrors `make_object`'s
  synthetic layout with a tiny fixed 3-class hierarchy (exact/subclass/
  interface/miss), not the real class table — `jit_typecheck_resolve`'s own
  subtype logic (loader-dup fallback, the primitive-array vs `Object[]` fix)
  has its own unit tests in `vm/src/jit/helpers.rs` and is exercised for real
  by calling the identical helper in production; this harness exists to prove
  the LOWERING (register ABI, safepoint handling, sentinel propagation), not
  to re-verify the helper.
* `ir_vs_singlepass_{instanceof,checkcast}_not_yet_loaded_refuses_ir` — a
  `Deferred` resolver answer must keep the site off the IR tier entirely
  (`used_ir_backend == false`) while single-pass still resolves and answers
  correctly.
* `ir_vs_singlepass_mixed_checkcast_and_instanceof_in_one_method` — the shape
  this lane newly allows: both opcodes in one method, at two different pcs
  sharing one cp index. Proves the pc-keyed split (not cp_idx-keyed) is
  correct, and that a checkcast failure occurring AFTER an instanceof/istore
  already ran still overrides the method's normal return with the sentinel.

## Residuals

* **`multianewarray`/`anewarray`/`newarray`** (`cov-06`) and **`athrow`**
  (`cov-07`) are untouched — both conjuncts still refuse their opcodes
  exactly as before, at lower absolute counts on this corpus per the table
  above (re-ranking, not a fix).
* **`invokedynamic`** (`scan.indy_ops`) has no owning lane. 38 refusals on
  this corpus, unowned before this lane and still unowned after.
* **Array/primitive-array instanceof semantics** were not re-derived —
  `Op::InstanceOf`/`Op::CheckCast` call the real production `jit_instanceof`/
  `jit_checkcast` helpers, so every existing fix there (the strict
  `byte[] instanceof Object[]` rule, SBR-03's lenient `checkcast` array
  carve-out, the loader-duplicate name-based fallback) applies automatically
  and was not re-tested by this lane's synthetic harness — see the "What the
  single-pass backend already has" framing this doc opened with; it held.
* **Not-yet-loaded targets stay on single-pass**, unconditionally. Widening
  that (a `Deferred`-shaped helper call mirroring `new`'s
  `new_object_cp`/`anewarray_object_cp` CP-indexed resolution path) is a
  real, un-sized follow-up — it would let a colder `checkcast`/`instanceof`
  site reach the optimizing tier, but means hosting a user classloader's
  `loadClass` inside a helper call from compiled code, which this lane
  deliberately declined to take on.

## Reproducing

```bash
CRATONVM_DBG=ir-compiles <cratonvm> ... SbRunner \
  org.springframework.boot.autoconfigure.condition.ConditionalOnPropertyTests
```

`grep -c 'checkcast_ops' <log>` is 0 on the fixed tree at any corpus; it was
169 on `ConditionalOnPropertyTests` alone before this lane, and 306 on the
original three-workload `ir-coverage-survey-20260803.md` corpus.
`regression-suite/perf/c2-reach.sh` gives the same `requests`/`admitted`/
`bodies` triple with its own witness cross-check (the tier manager's
`compiles: c1=… c2=… osr=…` shutdown line), for any workload, in one run.
