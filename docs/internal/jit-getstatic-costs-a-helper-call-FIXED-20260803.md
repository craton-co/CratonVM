# `getstatic` cost a helper call — FIXED 2026-08-03

**Status:** FIXED. A compiled `getstatic` is now two loads and no call, and the
marginal cost of a static read is at HotSpot parity (indistinguishable from
zero). Retired from `docs/known-issues/jit/`.

Every `getstatic` in compiled code used to `CALL jit_getstatic`. `getfield` did
not — it is inlined. HotSpot constant-folds a `static final` reference to its
address and emits a plain load. Rounds 1 and 2 trimmed the helper from ~52 ns
marginal to ~16 by deleting work *inside* it; round 3 deleted the call.

## Result

`probes/StaticFieldProbe.java`, 2M iterations, marginal ns/op over the call-free
control. `INLINE` and `HELPER` are the same binary, A/B'd by
`CRATONVM_JIT=getstatic-helper`, 5 interleaved pairs (every pair agreed to
±0.05; the medians are quoted):

| rung | HELPER (was) | INLINE (now) | HotSpot |
|---|---|---|---|
| `static final` REF | +13.84 | **−0.30** | −0.01 |
| `static` mutable REF | +13.85 | **−0.24** | −0.01 |
| `static` mutable int | +8.91 | **−0.55** | −0.01 |
| instance field (`getfield`) | +0.00 | +0.00 | −0.01 |
| `static final` REF, hoisted | −0.32 | −0.32 | −0.00 |

Absolute, for scale: `static mutable REF` 15.42 → 1.33 ns/op against a 1.57
ns/op control. The marginals are *negative* because the rungs' loop bodies are
not bit-identical to the control's, not because the read is free-plus-a-bonus —
the honest reading is "below this harness's resolution", i.e. the same answer
HotSpot gives.

The reference rungs are no longer dearer than the primitive one. That closes the
round-2 residual ("the `Value::Object` arm adds a `plausible_heap_pointer`
check"): with no helper there is no `Value` decode at all, only the payload
word.

All three arms — HotSpot, `INLINE`, `HELPER` — print the same checksum
(`sink=-1428027461125721286`).

A second 5-pair set taken later the same day, on a busier host (control drifting
1.7 → 2.8 ns/op between runs), read **+16.1 / +12.3** for the helper's REF and
int rungs and the same **−0.28 / −0.63** for the inline form. The helper's cost
tracks host contention — it is a call — while the inline form's does not. Read
the marginals, never the absolutes, and interleave: the same drift is what made
a single-shot A/B of round 1 look like it did nothing.

## What the fix is

Three pieces, one per blocker named in the old `x64.rs` bail comment.

**1. A stable address to bake.** Round 2 already replaced the `Vec<Value>`
inside the `RwLock`ed map with `StaticsBlock` — one leaked, never-freed
allocation per class — and mirrored it in the lock-free `StaticsIndex`. What the
backend bakes is *not* the block address: it is the address of the
`AtomicPtr<Value>` **cell** that names the block
(`StaticsIndex::base_cell_addr`). The slot array is allocated once and never
freed, so that address is valid for the life of the process, and it costs one
extra dependent load to be immune to every republication path — a `grow_to`
(a write past the published length) or a second `prepare_class_shared` swaps the
block, and already-compiled code follows it with no patching, no invalidation
protocol, and no new invariant anyone has to maintain. Baking the block address
would have been one load cheaper and would silently read an abandoned copy.

**2. A layout to read.** The `Value` cell is the same 16 bytes the inline
`getfield` arms have been reading since July: discriminant word at
`FIELD_CELL_TAG_OFFSET`, 4-byte payload at `FIELD_CELL_PAYLOAD32_OFFSET`, 8-byte
payload at `FIELD_CELL_PAYLOAD64_OFFSET`, pinned by
`types::heap_types::field_cell_layout_matches_value_enum`. Emitted shape:

```text
  MOV RAX, imm64                     ; &statics_index[class].base
  MOV RAX, [RAX]                     ; the class's statics block
  MOV/MOVSXD RAX, [RAX + idx*16 + p] ; the payload
```

`MOVSXD` for the int category (matching `Value::Int(i) => i as i64`), a 32-bit
zero-extending `MOV` for float (`f.to_bits() as i64`), a 64-bit `MOV` for
long/double/reference (`Object(None)` leaves that word zero, i.e. JVM null).
Volatile statics keep the `MFENCE` the helper arm emitted. Reference results
still get `mark_top_as_oop`.

**3. A way for the `jit` crate to ask.** It still has no `vm` dependency and did
not need one. The VM registers a resolver function pointer plus its own
`SharedVm` pointer through a process-global setter
(`x64::set_static_base_resolver`), exactly as it already registers the savebase
watch helpers — no `JitRuntimeHelpers` field, no golden offset, no ABI revision
bump, because generated code never calls it. Only the compiler does, while
emitting.

### What the resolver refuses, and why each refusal is load-bearing

`jit_resolve_static_base` answers `0` — "keep the helper" — for:

* **`java/lang/System`.** Its `out`/`err`/`in` statics are serviced by the
  bootstrap intercept inside `jit_getstatic`, which returns a synthetic stream
  rather than the stored value. A direct load would read the raw slot and
  `println` would silently no-op on a null stream. Answered from a new per-VM
  `ClassRealm::system_class_id`, recorded at preparation.
* **A class not yet initialized at compile time.** An inline load runs no
  `<clinit>` (JVMS §5.5); the helper's init check is the only thing between a
  compiled first-touch `getstatic` and the zero-initialized placeholder (this is
  the `LineWrapper$FlushType` NPE from July). A site is inlined only when the
  class is *already* initialized — which is exactly when HotSpot omits its init
  barrier too.
* **Nothing published yet, or `CRATONVM_JIT=-statics-index`.** No address to
  bake; the index kill switch stays a real A/B rather than one that stops
  applying as methods tier up.
* **Two VMs in one process.** `ClassId`s are per-VM. The context is latched to
  the first VM that registers, and a second registration poisons the mechanism
  permanently for both — every static read goes back to the helper. Slower,
  never wrong. Same failure mode, same remedy as `class_init_memo` and the
  deleted process-global `system_class_id` (`docs/vm-jit-cache-keying.md`).

The resolver takes **no VM lock**: three atomic loads. That is deliberate — it
runs on whichever thread is compiling, including the background compiler, and a
resolver that reached for `class_manager.read()` would be one queued writer away
from deadlocking a compile against a class load. Making that possible needed one
new hook: `finalize_class_init` now marks the lock-free `class_init_memo` on
every successful `<clinit>`. Before, that memo was written *only* by
`jit_getstatic` — i.e. only after a compiled static read had already taken the
slow path once, which is too late for a compiler that must decide before the
method's first compiled execution.

Kill switch: `CRATONVM_JIT=getstatic-helper` (legacy spelling
`CRATONVM_JIT_GETSTATIC_HELPER=1`), mirroring `getfield-helper`.

### A latent bug this removed

The helper arm routes `RAX == i64::MIN` through
`emit_post_invoke_exception_check` as a `<clinit>`-failure deopt sentinel. A
`static long` legitimately holding `Long.MIN_VALUE` is indistinguishable from
that. The inline form makes no call, so it has no sentinel and no ambiguity. The
helper path is still reachable (see the refusals above) and still has it.

## The measurement trap: this probe was measuring the interpreter

Worth more than the fix. Run as its old reproduction line said, every rung read
**~90 ns/op, identical under `--nojit`** — because 200 warm-up invocations per
rung is below the tier-up threshold (`c1_threshold=500`), so the only route into
compiled code was OSR, and **every OSR entry is refused today**:

```
OSR-refuse StaticFieldProbe.staticMutInt(I)J entry_pc=4 JIT bailout
  [unsupported_shape]: osr-entry-unresumable-exit
  (deopt point at bci 1 (OsrExit) reconstructs an unresumable frame) (memoed)
```

Not specific to this probe — `VirtOnlyProbe`'s four rungs are refused the same
way, 406 times each. Filed separately as
`docs/known-issues/jit/osr-entry-unresumable-exit-refuses-hot-counted-loops-20260803.md`;
it is not a `getstatic` problem and this fix does not touch it.

`probes/StaticFieldProbe.java` now warms up 1200 invocations per rung, so the
rungs tier up normally and the numbers above are compiled-code numbers.

> A microbenchmark that never says which tier it measured will eventually
> measure the wrong one. The tell here was cheap and was not checked for two
> rounds: **the control rung.** At 90 ns/op for `acc += i ^ (acc >>> 7)` against
> HotSpot's 0.84, nothing in that run was compiled. Check the control against
> HotSpot before reading a single marginal, or run
> `CRATONVM_DBG=jit-method-stats`.

This also means the round-1/round-2 tables in this document's history are not
comparable to the round-3 table: they were taken before the OSR refusal existed,
against a probe that then really did run compiled. The 07-31/08-01 *relative*
findings (the intercept was the dominant segment; the lock-free index changed
nothing) stand — they were interleaved A/Bs of the same binary shape — but do
not read their absolute ns/op alongside the table at the top.

## What this does NOT cover

* **`putstatic` stays on its helpers.** The address machinery would serve a
  write just as well, but the write side is not symmetric: `set_static_shared`
  fires the SATB pre-barrier for an overwritten reference (statics live outside
  the heap, so no collector `set_field` barrier covers them — a missed one is a
  hidden-pointer SATB hole, i.e. a live object freed) and it is also what
  creates or grows a class's block on first touch. Inlining reads costs that
  machinery nothing; inlining writes would have to reproduce all of it. A
  primitive-only inline `putstatic` is the tractable next step if a workload
  ever shows static writes on a hot path — reads were the measured problem.
* **The OSR refusal above.** Separate doc, separate area.
* **`getstatic` from the interpreter.** Unchanged.

## History

* **Round 1 (2026-07-31)** — halved. `java/lang/System`'s `ClassId` resolved
  once and compared as an integer instead of a per-read `class_manager.read()` +
  name compare; class-initialization memoized in a lock-free bitmap. 52 → ~36
  ns marginal.
* **Round 2 (2026-08-01)** — profile first, then fix. `CRATONVM_DBG_GETSTATIC_PROF=1`
  timed each segment with `rdtsc` and reported whether the lock-free index was
  actually hit. Two results: the `RwLock` + hash probe were **not** the
  bottleneck (the new `StaticsIndex` showed 4.8M hits against 11 misses and
  **no wall-clock change** — kept anyway, it is the prerequisite for baking the
  address); and the `System.out` intercept was still the dominant segment at 98
  cyc/call — the very thing round 1 believed it had fixed, because resolving
  System's id by NAME only helps if that lookup resolves, and when it does not
  every call still takes the lock and looks exactly like working code. Replaced
  with a per-`ClassId` memo: 98.2 → 20.8 cyc, the instrumentation floor.
  ~16 ns marginal.
  > A guard whose cost is invisible from outside will be "fixed" twice. Measure
  > the segment, not the function.
* **Round 3 (2026-08-03, this one)** — no call at all.

### The `getstatic`-in-a-microbenchmark warning, still worth keeping

This was found while decomposing an apparent 6x `invokevirtual` penalty that was
largely **not dispatch**: the probe called `LEAF.addOne(i)` where `LEAF` is a
`static final` field, so every iteration paid a hidden `getstatic` helper call
the `invokestatic` rung never paid. Hoisting the receiver into a local took
`invokevirtual` from 42.03 to 9.44 ns/op. With the receiver hoisted, virtual
dispatch cost only ~2.7 ns more than a direct static call — 1.5x, not 6x. That
particular trap is now defused (the read is free), but the habit is not: hoist
first, then measure.

Also still true: **a bare direct call costs ~5 ns on this VM**
(`probes/VirtOnlyProbe.java`, `invokestatic` marginal). No amount of trimming
inside a helper can beat that — which is why the fix had to delete the call.

## Reproduction

```bash
cratonvm --java-home <jdk25> -cp <probes> StaticFieldProbe 2000000
CRATONVM_JIT=getstatic-helper cratonvm --java-home <jdk25> -cp <probes> StaticFieldProbe 2000000
<jdk25>/bin/java -cp <probes> StaticFieldProbe 2000000
```

Interleave the first two, several times: the host these were taken on is shared,
and a single-shot A/B of round 1 showed it doing nothing because the control had
drifted 1.74 → 3.04 ns between runs.

Code: `jit/src/x64.rs::try_emit_inline_getstatic` and the `0xb2` arms (top-level
and inlined-callee), `jit/src/x64/licm.rs::{set_static_base_resolver,
resolve_static_base, inline_getstatic_enabled}`,
`vm/src/jit/helpers.rs::{jit_resolve_static_base, note_class_initialized}`,
`vm/src/vm/realms/class_realm.rs::StaticsIndex::base_cell_addr`.
Test: `jit/src/x64.rs::test_getstatic_inline_direct_load_and_fallback`.
