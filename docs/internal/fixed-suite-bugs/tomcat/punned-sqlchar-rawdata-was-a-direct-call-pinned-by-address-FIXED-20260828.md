# `SQLChar.rawData` holds `Int(1)`: a raw direct call was pinned by ADDRESS, and the address had changed hands

| | |
|---|---|
| **Status** | **FIXED**, 2026-08-28, on `fix/punned-sqlchar-init-writer-20260827` |
| **Symptom** | a `[C` field (slot 1 of an 8-slot `SQLChar`) holding `Value::Int(1)`; `org.apache.catalina.servlets.TestWebdavPropertyStore` fails with `NullPointerException` at `StoredPage.readRecordFromArray`, and before the detector existed, `SIGSEGV addr=0x5` |
| **Defect** | `JitCache::prepare_for_publication` pinned each baked direct-call target by its ENTRY ADDRESS. A callee retired between the caller baking `CALL E` and the caller reaching publication did not fail to pin — it pinned whatever body owned `E` by then |
| **Fix** | record the callee's `artifact_id` at bake time and compare identity, not address, at publication; a mismatch is treated as an unpinnable target, which `strict_callee_roots` already refuses to publish |
| **Rate** | ~3-7% of runs, **only under CPU contention** |

## What the chain was

One run's log, in order, with the entry address `E` fixed:

```text
13713: full-compile SQLChar.<init>()V     entry=E len=1362
13714: bg-direct-call BOUND SQLChar.<init>()V          <- a caller bakes CALL E
14055: [jit-code-free] base=E  published=true authorised=false
14185: full-compile SQLInteger.<init>()V  entry=E len=657    <- E recycled
14264: full-compile DataValueFactoryImpl.getNullDVDWithUCS_BASICcollation
14701: [punned-store-jit] class=SQLChar field_index=1 value=Int(1)
       compiled_frames=[E+0x224, getNullDVDWithUCS_BASICcollation+0x7ab]
```

`SQLInteger.<init>()V` opens `aload_0; iconst_1; putfield isnull:Z` — field
index **1**, value **1**, through `jit_putfield_int`. `SQLChar`'s slot 1 is
`rawData`, declared `[C`. So a caller that resolved `SQLChar.<init>` ran
`SQLInteger.<init>`'s body on a `SQLChar`, and Derby later read `rawData` as
the pointer `1`.

Six Derby types have that exact shape and any of them can be the tenant:
`SQL{Boolean,Double,Integer,Longint,Real,Smallint}.<init>()V`.

## Why the existing guard did not catch it

`prepare_for_publication` already refuses to publish a body whose baked
direct-call targets cannot be kept alive, and `strict_callee_roots` has been
default-ON since 2026-07-28. It asked the wrong question:

```rust
.filter_map(|&entry| resolve_jit_entry_owner(entry))
```

`resolve_jit_entry_owner` answers *"is something live at this address"*.
`JIT_ENTRY_OWNERS` is a map keyed by entry address and a recycled buffer
**overwrites** the entry with its new tenant, so after a recycle the lookup
succeeds — on the wrong artifact. The pin then holds a strong reference to
`SQLInteger.<init>`, publication is allowed, and the caller's `CALL E` runs
that body for the rest of its life.

`[jit-unrooted-callee]`, the counter for the case the guard *does* catch, is
**anti-correlated** with the defect: 0 of 16 hit runs against 232 of 284
non-hit runs. A failed pin is the safe case — the body is not published.

**An address comparison is not enough either.** A first version of this fix
recorded `(entry, cm_ptr)` at bake time and compared pointers. Its refusal
never fired once in 300 runs and the defect survived: the freed `Arc`'s
allocation is recycled by the global allocator as readily as the executable
buffer, so the two artifacts compare EQUAL. That is an ABA, and it is why the
fix carries a monotonic `CompiledMethod::artifact_id` instead.

## The fix

* `CompiledMethod::artifact_id` — a process-unique monotonic id, handed out at
  construction. Neither the entry address nor the `Arc` address identifies a
  body; both are recycled.
* `CompiledMethod::_direct_callee_expected: Vec<(usize, u64)>` — for every raw
  direct CALL baked into this body, the address AND which artifact owned it
  when it was baked. Populated at all four bake sites (the single-pass driver's
  two, the IR path, and `intern_inline_invoke_targets`).
* `prepare_for_publication` compares `owner.artifact_id` against the recorded
  id. A mismatch counts `REBOUND_DIRECT_CALLEES`, logs `[jit-rebound-callee]`
  under `CRATONVM_DBG_JIT_PIN=1`, and drops the root — which routes into the
  existing `strict_callee_roots` refusal. The method stays interpreted and
  recompiles later against a live callee, exactly as it already did for an
  unrootable target.

`lambda_adapter` never had this hole: it captures the callee's `Arc` at bake
time rather than re-deriving one from the address later. It now records the
identity too, so an adapter is checked the same way.

## The measurement

The A/B is on ONE binary, and the disabled arm still performs the same
bake-time lookup (and takes the same `jit_entry_owners` lock), so the result is
the check and not its overhead — this session had already watched a `Mutex` in
a *diagnostic* make the defect vanish, which is why that control exists.

| arm (binary `cv-disc`) | runs | punned stores | runs failed |
|---|---:|---:|---:|
| `CRATONVM_JIT_CALLEE_IDENTITY=0` (lookup kept, identity not recorded) | 300 | **11** | 13 |
| default (identity recorded and compared) | 300 | **0** | 0 |
| default, repeat | 300 | **0** | 0 |

Adjacent cross-binary control, same host and hour: the unfixed build gives
8 stores / 9 failures per 300. The refusal fires — 13 `[jit-rebound-callee]`
events in 4 runs — so the zero is a guard engaging, not a quiet arm.

Also green with the fix: `regression-suite/run.sh` **72 passed, 0 failed**.

## Reproduction

The rate is zero on a quiet host. Four CPU spinners are the whole difference:
600 runs quiet produced nothing; 300 runs contended produce 8-21.

```bash
for i in 1 2 3 4; do (while :; do :; done) & done
scripts/punned-cell-campaign.sh <cratonvm> <tag> 300 4
```

## What it took, and the five hypotheses that died first

Recorded because each looked conclusive, and because the instrument that
finally answered it is the one that should have been built first.

1. **Buffer recycling with a stale direct call.** Right in the end — but the
   test that "refuted" it was wrong: it asked whether `SQLChar.<init>` and a
   sibling ever share an entry address anywhere in a run. They do, in 4/4 hit
   runs *and 291/296 non-hit runs*. Address reuse is ubiquitous; what matters
   is reuse **between bake and call**, which only the chronological ordering
   shows. Set membership was the wrong shape of question and it cost the
   hypothesis half a day.
2. **A missing keep-alive.** Anti-correlated, as above.
3. **A pin succeeding on the wrong artifact, compared by pointer.** ABA; never
   fired.
4. **A wrong bind at compile time.** `CRATONVM_DBG_JIT_DIRECT_BINDS` prints
   every baked call with its callee triple and entry, cross-checked against
   `full-compile … entry=…`: no mismatch. The binds are right when made.
5. **Scalar replacement / the trivial-ctor elision as the corrupter.**
   `CRATONVM_JIT_ELIDE_TRIVIAL_CTOR=0` and `CRATONVM_JIT_SCALAR_NEW=0` both
   take it to zero, and neither is the defect: the elision makes these
   constructors small and churn-prone, so their buffers recycle among one
   another. They move the exposure, not the cause. Eleven other levers moved
   nothing at all (relocation, inline putfield, inline new, compact layout,
   cached-entry owner reuse, the code-cache cap, scalar replacement proper, the
   dispatch-cache direct entry, an 8g heap, denying one of the six).

**What ended it** was making the report name the executing body exactly. The
conservative stack scan that shipped first found frames sometimes and nothing
other times on the same binary — silence that meant nothing. Rebuilt as an RBP
chain walk (with `-C force-frame-pointers=yes`), it named one frame on the
positive control and, on the fault, `E+0x224` under
`getNullDVDWithUCS_BASICcollation+0x7ab` every single time. Ordering that
against `full-compile` and `[jit-code-free]` in the same log gave the sequence
at the top of this page in one run.

## Instruments left behind

All env-gated and off by default.

* `CRATONVM_JIT_CALLEE_IDENTITY=0` — the fix's own kill switch, and the control
  arm above.
* `CRATONVM_DBG_JIT_PIN=1` — `[jit-rebound-callee]` and `[jit-unrooted-callee]`.
* `CRATONVM_DBG_JIT_FIELD_SITES=<method substring>` — every field site the
  single-pass backend emits (method, pc, slot, tag), plus `[jit-kept-callees]`
  and `[jit-emit-direct]`. This is what proved no `SQLChar` method emits an int
  store to slot 1, and that six sibling constructors do.
* `CRATONVM_DBG_JIT_DIRECT_BINDS=<caller substring>` — every baked direct call
  with callee triple and entry.
* `CRATONVM_DBG_JIT_EA=<method substring>` — escape-analysis and elidable-init
  decisions per method.
* `CRATONVM_JIT_ELIDE_TRIVIAL_CTOR=0` / `CRATONVM_DBG_JIT_ELIDE_CTOR` — the
  `invokespecial C.<init>()V` → `java/lang/Object.<init>()V` rewrite.
* `compiled_frames=` on `[punned-store-jit]`, from the RBP chain.
* `scripts/punned-cell-campaign.sh` — the contended campaign, including the
  spinners without which it measures nothing.
