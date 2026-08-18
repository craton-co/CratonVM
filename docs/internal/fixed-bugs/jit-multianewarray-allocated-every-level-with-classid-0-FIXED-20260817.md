# The JIT's `multianewarray` allocated every level with `ClassId(0)`, so `new String[a][b]` read back as `[Ljava.lang.Object;`

**Status: FIXED 2026-08-17** on `fix/jit-precise-root-map-checkcast-20260817`.
Regression vector: `regression-suite/src/RJitMultiArrayClass.java` (21 shapes ×
cold/hot/tier-split = 63 checks). Source witnesses:
`cratonvm_jit::tests::multianewarray_lowering_passes_the_resolved_site_not_an_element_type`,
`…::multianewarray_helper_calls_the_shared_interpreter_body`,
`…::multianewarray_site_packing_round_trips`, and
`interpreter::tests::multianewarray_arm_guards_the_component_bracket_subtraction`.

This retires `docs/known-issues/jit/checkcast-on-atomicreference-get-reads-a-reclaimed-array-20260817.md`,
whose diagnosis — a GC precise-root-map / safepoint-publishing gap — was
**wrong**. See [What the original diagnosis got wrong](#what-the-original-diagnosis-got-wrong);
it is worth reading, because the reasoning that produced it was locally sound
at every step.

## The bug

`jit/src/x64/bytecode_walk.rs`'s `0xc5` arm lowered `multianewarray` to
`jit_multianewarray_2d`, and the only type information it passed was a leaf
**element type** code:

```rust
let leaf_et = self.multianewarray_info.iter().find(|(p, _)| *p == pc)
    .map(|(_, et)| *et as i32)
    .unwrap_or(10);                      // default T_INT
```

An element type is `T_INT` / `T_DOUBLE` / "reference". It names no class. So
the helper could only do this:

```rust
let outer = heap.alloc_array(ClassId::new(0), ArrayElementType::Reference, dim1);
for i in 0..dim1 {
    let inner = heap.alloc_array(ClassId::new(0), elem_type, dim2);
    ...
}
```

`alloc_array`'s first argument is the array's **component class id**. With
`ClassId(0)` the allocated object carries no array class, and `getClass()`
falls back to the generic `[Ljava.lang.Object;`. A JIT-compiled
`new String[a][b]` therefore produced an object whose runtime class was
`[Ljava.lang.Object;` — one dimension, wrong element — where the source, the
verifier and the interpreter all say `[[Ljava/lang/String;`.

The interpreter's `Instruction::Multianewarray` arm resolved the per-level
component classes correctly and **said so in its own comment**:

> Without this every multi-dim array was allocated with `ClassId(0)` and
> `getClass().getName()` collapsed to `[Ljava/lang/Object;`.

That comment was written when the interpreter half was fixed. The JIT half was
never touched, and nothing in the build linked the two — the same
one-contract-transcribed-twice shape as
`W7-38-jit-aastore-never-called-its-own-check.md`.

## Why it stayed invisible

Nothing about a wrong array class is observable until something asks for the
type. Almost nothing does: `aaload`, `aastore` (a `ClassId(0)` array has no
component type, so the covariance check could not refuse anything either),
`arraylength` and ordinary element access all work fine on a classless array.
Two things do ask, and both were in the wild:

* **a `checkcast` back to the declared array type.** Commons Math's
  `DSCompiler.getCompiler` publishes a `DSCompiler[][]` through an
  `AtomicReference<DSCompiler[][]>` and casts it back on the next call — the
  erasure `checkcast` javac emits for `AtomicReference<T>.get()`. Once
  `getCompiler` was compiled, the array it published had no class, and every
  later call threw. 118 of `DerivativeStructureTest`'s 124 methods, and the
  cascade is inherent: the poisoned array lives in a `static` field, so one
  corruption event fails every subsequent test in the class.
* **`ObjectInputStream`'s reflective field restoration**, which rejects an
  array whose runtime class does not match the declared field type. That is
  `NordsieckStepInterpolatorTest.serialization`: `Array2DRowRealMatrix.data`
  is declared `[[D` and `FieldReflector.setObjFieldValues` was handed
  `[Ljava.lang.Object;`. No bytecode `checkcast` is involved at all, which is
  exactly why the original write-up read it as evidence for a general
  GC-level defect.

## Diagnosis

Four measurements, in the order that made the rest cheap:

1. **`CRATONVM_JIT_DENY` bisect.** `org/apache/commons/math4` → 124/124.
   Narrowing to `…DSCompiler.getCompiler` alone → still 124/124. One compiled
   method, named in four runs.
2. **Not GC.** `--Xmx 8g` (no collection pressure) and `--XX:UseGc G1` both
   reproduce it **identically**, 118 failures each. A root-publishing bug does
   not survive removing the collector's opportunity to run.
3. **Not a race.** Three consecutive runs produced byte-identical failure
   *sets*, not merely equal counts.
4. **A standalone probe.** `new String[a][b]` in a loop, checking
   `getClass().getName()`, diverges at **iteration 515** — just past the
   default `CRATONVM_TIER_C1_THRESHOLD=500`. `new double[a][b]` and
   `new String[a][b][c]` were correct in the same probe, because the x64 scan
   only admits `multianewarray` with `dimensions == 2` and the interpreter
   handles the rest.

## The fix

The interpreter's body is **extracted**, not copied, into
`interpreter::multianewarray_alloc`, and both tiers call it. That is the whole
point: two transcriptions drifting is the defect, so the repair has to remove
the second transcription rather than resynchronise it.

The backend now hands the helper the site's `(holder_class_id, cp_idx)` —
packed into one i64 by `cratonvm_jit::pack_multianewarray_site`, because
Windows x64 gives a helper four register arguments and three are already spent
on `(vm_ptr, dim1, dim2)`. The helper resolves the array class from that pair
**on the executing thread**, which is the same shape
`jit_anewarray_object_cp` / `jit_new_object_cp` already use and for the same
reason: resolution defines array classes and can run a user
`ClassLoader.loadClass`, i.e. arbitrary Java, which a background compile thread
must not do.

Two latent defects in the same arm went with it:

* a negative dimension returned a bare `0` that the compiled code pushed as
  null and then dereferenced. The arm now emits `emit_post_alloc_oom_check`,
  so a `NegativeArraySizeException` (and a real OOM) routes through the
  method's exception table like every other allocation site;
* the arm never republished the frame after its call. It can now run Java, so
  it emits `emit_post_call_frame_republish` like the `new`/`anewarray`
  CP-indexed arms.

### The memo, and the two hazards in it

Resolving per execution costs a `class_manager` read lock, a `get_class_name`,
two `String` builds and up to two loader-aware resolutions. Measured on a
300 000-iteration `new String[4][4]` loop, interleaved arms, best-of-7:

| binary | `String[4][4]` | `double[4][4]` |
|---|---|---|
| pre-fix (wrong class) | 418 ns/op | 285 ns/op |
| fixed, no memo | 645 ns/op (+55%) | 405 ns/op (+42%) |
| fixed, memoized | 478 ns/op (+14%) | 318 ns/op (+11%) |
| HotSpot 25.0.3+9 | 12 ns/op | 16 ns/op |

The residual +11–14% is the price of the correct array class, on an opcode
that is ~30× off HotSpot for reasons this change does not touch.

Two things about the cache are load-bearing and neither is obvious:

* **Class ids are recycled.** `unload_user_classes` can retire `p/X` under a
  live loader and let that loader define a fresh `p/X` with the same id — the
  exact aliasing `ClassManager::array_class_for` re-synthesises to avoid. A
  plan holds ids on *both* sides (the key names the referencing class, the
  value names each level's component class), so it is invalidated wholesale
  from `memory::gc`'s unload path, next to the `jit_alloc_class_cache` and
  `profile_store` invalidations that were already there.
* **The read guard must be dropped before allocating.** `alloc_multi_array`
  can trigger a GC, and the GC's own unload path is what takes this lock for
  *write*. Holding the guard across the allocation is a deadlock edge through
  a `parking_lot::RwLock` that is neither reentrant nor writer-starving. The
  hit path therefore clones an `Arc` and drops the lock — one refcount bump,
  measured perf-neutral against the version that held it.

A plan is inserted **only when every level that names a class actually
resolved**. Caching a `ClassId(0)` fallback would make a transient resolution
failure permanent, which is the same defect one layer down.

## What the original diagnosis got wrong

The known-issue doc concluded "a live reference at a `checkcast` program point
in JIT-compiled code is not always included in the precise root map". Every
step that produced it was locally reasonable, and it is worth naming which
steps carried the error:

* **The guard was believed over the arithmetic.** `reclaim_guard`'s
  `report_root_slice_provenance` fired with `in_published_snapshot=false`, and
  that line's own doc comment says a `false` means "the snapshot is missing a
  slot the thread's frames hold". But the reporter only runs *after*
  `report_reclaimed_receiver` has decided the address sits in a reclaimed
  hole, and that decision is about the ADDRESS's history — see
  `reference_a_gc_guard_reclaimed_hit_is_about_the_address's_history`. A
  classless array whose header decodes as the wrong class trips the same path
  as a genuinely reclaimed one. **The instrument named the defect it was
  built to name.**
* **The negative controls were never taken.** `--nojit` fixing it was read as
  "JIT-only, therefore a JIT/GC interaction". The two controls that would have
  separated *JIT* from *GC* — a heap large enough that no collection runs, and
  a second collector — were not run. Both take one command and both come back
  negative.
* **"Race" was inferred from a repro that did not reproduce.** The
  hand-written `MultiArrayRepro`/`DSCompilerDriver`/`DSCompilerHot` probes
  passed, and that was read as timing-dependence. They passed because none of
  them ever asked for the array's runtime class after compilation:
  `DSCompilerHot` warms `getCompiler(2,1)` 50 000 times and then makes ONE
  growth call, so the `checkcast` on the corrupted array never runs a second
  time. A probe that cannot observe the defect is not evidence of a race.
* **Determinism was not measured.** The doc reports "~10 of 124". The same
  class on this branch's baseline binary fails 118 of 124 with an
  **identical failure set across three runs**. A stable set is the single
  cheapest discriminator between a race and a logic error, and it was one
  `diff` away.

The second witness (`NordsieckStepInterpolatorTest`, through
`ObjectInputStream` rather than a `checkcast`) was read as the strongest
evidence for the general-root-map reading — "the leak is upstream of any
specific bytecode-level instruction". It was in fact strong evidence for the
correct answer too, and more specifically so: both consumers were asking the
same question, *what class is this array*, and getting the same wrong answer.
"Two different readers disagree with the type system about one object" points
at the object's construction before it points at the collector.

## Reproduction (pre-fix)

```bash
CV=target/release/cratonvm.exe
JDK="<jdk25>"
# The whole defect, in one file — no suite, no classpath:
cat > MANew.java <<'EOF'
public class MANew {
    static Object mk(int a, int b) { return new String[a][b]; }
    public static void main(String[] a) {
        for (int i = 0; i < 200000; i++) {
            String n = mk(2, 3).getClass().getName();
            if (!n.equals("[[Ljava.lang.String;")) { System.out.println("i=" + i + " " + n); return; }
        }
        System.out.println("PASS");
    }
}
EOF
"$CV" --java-home "$JDK" -c . MANew     # pre-fix: "i=515 [Ljava.lang.Object;"
"$CV" --java-home "$JDK" --nojit -c . MANew   # PASS
```

Or the maintained form, both ways:

```bash
CV=<binary> JDK=<jdk25> ONLY=RJitMultiArrayClass bash regression-suite/run.sh
```

## Measured

All against HotSpot 25.0.3+9 in the same run, `CratonRunner`, `--Xmx 1g`:

| class | pre-fix | fixed | HotSpot |
|---|---|---|---|
| `DerivativeStructureTest` | 6/124 | **124/124** | 124/124 |
| `FunctionUtilsTest` | 8/15 | **15/15** | 15/15 |
| `FiniteDifferencesDifferentiatorTest` | 5/15 | **15/15** | 15/15 |
| `NordsieckStepInterpolatorTest` | 1/2 | **2/2** | 2/2 |
| `Array2DRowRealMatrixTest` | 39/39 | 39/39 | 39/39 |
| `DSCompilerTest` | 7/7 | 7/7 | 7/7 |
| `SparseGradientTest` | 120/120 | 120/120 | 120/120 |

`regression-suite/run.sh`: 60 pass → **61 pass**. The two remaining failures
(`RImmutableFactoryTypes`, `RJdkIntrinsics3`) fail identically on the pre-fix
binary and are pre-existing on dev, unrelated to this change.

`RJitMultiArrayClass` was verified RED on the pre-fix binary (18 of 21 shapes
diverge; `s00` shows the tier split at `i=743`) and green under `--nojit` on
that same binary — the discrimination the vector exists for. The three Rust
source witnesses were each verified to fail when the defect is reintroduced.

## Related

* `docs/known-issues/jit/osr-refuses-any-method-with-an-exception-table-20260817.md`
  and the other `docs/known-issues/jit/` siblings from the same Commons Math
  sweep — different mechanisms, still open.
* `apps/commons-math/RESULTS-20260817.md` — the suite run this was found from.
  Its verdict table lists four of this doc's witness classes under the retired
  root-map reading.
* `W7-38-jit-aastore-never-called-its-own-check.md` — the same shape: one
  JVMS rule implemented twice, the JIT's copy missing the half the
  interpreter's had.
