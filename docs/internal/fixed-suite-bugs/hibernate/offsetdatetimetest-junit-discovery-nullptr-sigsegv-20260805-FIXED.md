# `OffsetDateTimeTest` — SIGSEGV / `NoSuchMethodError` during JUnit discovery — FIXED

**Status: FIXED (2026-08-05).** Root-caused, fixed, and re-verified with
interleaved A/B arms on one binary pair. Two defects were found and closed:

1. **The crash** — per-thread JIT dispatch memos are keyed on a
   `JitInvokeInfo` *address*, and those boxes are freed with their
   `CompiledMethod`. A recycled address let one call site serve a different
   one's resolution. Four of the eight site-keyed memos were flushed by
   neither invalidation trigger. `vm/src/jit/helpers.rs`.
2. **An API-fidelity defect found on the way** — `Class.getDeclaredAnnotations()`,
   `Class.getAnnotations()` and all three `getAnnotationsByType` natives built
   their result arrays with `java.lang.Object` as the component type.
   `native-builtins/src/lang_class.rs`. Independently reproducible, and NOT
   the cause of the crash (measured — see §4).

The original open page's hypothesis ("a JIT codegen bug: a null check … elided
or miscompiled") was **wrong**, and §2 records how it was falsified rather than
argued away.

---

## 1. What the original page had, and what it got wrong

The filed symptom was a single `rc=139` in a 4-shard residual rerun:
`EXCEPTION_ACCESS_VIOLATION … read at address 0x000000000000000E`, no
`@@RESULT` line, inside JUnit5 discovery
(`ReflectionUtils.findMethods` → `AnnotationSupport.findAnnotatedMethods` →
`LifecycleMethodUtils.findAfterAllMethods`). It correctly ruled the
`ClassId(0)` stale-address family out (fault address is `null_base + 0x0E`, and
zero young collections had run), and correctly concluded "not that family".

It then guessed **null-check elision in JIT codegen**. That is refuted by the
compiled body itself (§2): the null check is emitted, and it *passes* — the
faulting register holds `1`, not `0`.

The page's four "not yet established" items are all now established:

| Question | Answer |
|---|---|
| Does it reproduce? | Yes — 20–45 % of runs, in ~2 s each, via `DiscoveryProbe` (§3). |
| Which compiled method? | `AnnotationUtils.findRepeatableAnnotations([Ljava/lang/annotation/Annotation;…)V`, offset `0x1A8`. |
| Is `OffsetDateTimeTest` relevant? | **No.** Nothing in the class matters; it is JUnit's annotation walk that is exposed. Its own bodies never ran. |
| Does `--nojit` avoid it? | Yes, 12/12 clean — but that is *not* because codegen is wrong; the memos are only consulted from the JIT dispatch helpers. |

## 2. Falsifying the codegen hypothesis

`CRATONVM_DBG_JIT_NAMES=1` named the faulting method;
`CRATONVM_DBG_DUMP_JIT=findRepeatableAnnotations` dumped its 44 263-byte body,
and the faulting `pc` minus the code-range base is offset `0x1A8`:

```
0191  mov  rax, r12                 ; astore 6  (arg0 = Annotation[] candidates)
0194  mov  [rbp-0x38], rax
019f  test rax, rax
01a2  je   0x98d3                   ; NULL CHECK — present, and TAKEN when null
01a8  mov  eax, dword ptr [rax+0xc] ; arraylength          <<<< FAULT
```

The null check is emitted and it passes, so `rax != 0`. The fault address is
`0x0D`/`0x0E` and the array-length field lives at `+0xC`, so the incoming
`rax` was **`1`** (and `2` in the original report — the registers in that
banner show `rax=rbx=r8=r12=0x2`). An `int` had been delivered where an
`Annotation[]` belongs.

The prologue maps `rdx → arg0`, and the crash banner confirms `rdx=0x1` while
`r8`, `r9`, `rbx`, `r15` (args 1, 2, 4, 5) all hold plausible heap pointers.
**Only argument 0 was wrong**, and argument 0 is the result of
`element.getDeclaredAnnotations()` / `element.getAnnotations()` — an
`invokeinterface` on `AnnotatedElement`.

Two more faces of the same call, from the same probe:

```
NoSuchMethodError: java.lang.Object.annotationType()Ljava/lang/Class;
NoSuchMethodError: java.lang.Class.annotationType()Ljava/lang/Class;
```

A real method name resolved against a class that never declared it. Same
shape: the site's own name, someone else's class.

**Two levers were tried and both proved the mechanism was elsewhere** — each
was checked for liveness first, per the "an inert lever is not an elimination"
rule:

* `CRATONVM_JIT_NO_INLINE_IC=1` (temporary, since removed) suppressed MIC/PIC
  slot allocation so every compiled virtual site took the resolving helper
  instead of the emitted inline cascade. **Proven live**: the same method
  compiled to 32 794 bytes instead of 44 263. The crash still reproduced
  (3/20), so the machine-code cascade is not the mechanism.
* `CRATONVM_TIER_C2_THRESHOLD=100000000`. **Inert** — the compiled body sizes
  were byte-identical with and without it, i.e. this method is single-pass,
  not optimizing-tier, so no conclusion was drawn from it.

`CRATONVM_JIT_NEVER_FREE_CODE=1` was the first lever that moved the number
(9/20 → 5/20 anomalies, SEGV 3 → 0), which is what pointed at code lifetime
rather than code generation.

## 3. The repro that made this tractable

The original page's repro runs the whole class: ~7 minutes per attempt. The
crash is in *discovery*, so discovery alone is enough — and
`apps/hib-suite-runner/DiscoveryProbe.java` already existed:

```bash
cd apps/hib-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_DBG_JIT_NAMES=1 <cv> \
  --java-home "<jdk25>" --Xmx 1500m @common.args -Dcraton.batch=1 \
  DiscoveryProbe org.hibernate.orm.test.type.temporal.OffsetDateTimeTest
```

~2 s per run, 20–45 % hit rate. One caution that cost time: on **both**
CratonVM and HotSpot this probe prints `@@DISCOVERED tests=0 containers=1`,
because the class is a JUnit 6 `class-template` whose children materialise at
execution time. `tests=0` is the **pass** signal here, not a symptom.

`CRATONVM_BG_COMPILE=0` was not usable as a control on this fixture: it failed
100 % of runs with an unrelated `ExceptionInInitializerError` out of
`jdk/internal/constant/PrimitiveClassDescImpl` reached from
`VarForm.initMethodTypes`. That 100 % rate briefly read as a deterministic
reproduction of *this* bug; it is a different defect entirely — the eager
first-call compile door ran `ConstantDescs.<clinit>`, whose artifact's
`static_init_classes` pre-walk hoisted a line-249 class-initialization trigger
to method entry and inverted the JDK's own circular-init cycle. Taken on and
**FIXED 2026-08-05**:
`../jit/bg-compile-off-clinit-first-call-compile-order-20260805-FIXED.md`.
The opt-out now runs this fixture 5/5 clean, so it is usable as a control
again.

## 4. Root cause

`vm/src/jit/helpers.rs` memoizes dispatch decisions per JIT call site. The key
is [`JitSiteKey`] `= (SharedVm::vm_identity, JitInvokeInfo pointer)`. The
`vm_identity` half is well documented and correct. **The pointer half is not
stable for the life of the process**: `JitInvokeInfo` boxes are owned by
`CompiledMethod::_jit_invoke_infos` and, per that field's own doc, "are freed
when this `CompiledMethod` is dropped". The allocator is then free to hand the
same address to the next compile's `JitInvokeInfo` — at which point the key
names a *different* call site while every memo still holds the old site's
answer.

What the old answer is decides the face:

* **`NATIVE_SITE_CACHE`** holds a resolved leaf-native callback. The reused
  site CALLs the previous site's native and returns whatever that returns — a
  `boolean` `1` landing in a slot the caller then treats as an `Annotation[]`,
  whose `arraylength` reads `1 + 0xC` = `0x0D`. **That is the SIGSEGV.** Its
  receiver-class guard does not help when both sites share a receiver class,
  and `java/lang/Class` is one of the most common receivers in a reflection
  walk.
* **`VIRTUAL_TARGET_CACHE`** holds the resolved dispatch **class name**. The
  reused site resolves its own (correct) method name against the previous
  site's class. **That is `NoSuchMethodError: java.lang.Object.annotationType()`
  / `java.lang.Class.annotationType()`.**
* **`OBJECT_NATIVE_DISPATCH_CACHE`** / **`INTEGER_NATIVE_DISPATCH_CACHE`** hold
  the same shape of decision for their own fast paths.

Only `DISPATCH_CACHE` and `VIRTUAL_DISPATCH_CACHE` were being revalidated,
because the hazard had been framed narrowly as "a raw entry pointer can go
stale" (`flush_raw_entry_dispatch_caches`'s original name and doc). The
address-reuse hazard is broader: **a memo does not have to hold a code pointer
to be wrong once its key stops identifying its site.**

### The fix

The JIT cache generation closes it exactly. Publishing the `CompiledMethod`
that owns the reused `JitInvokeInfo` is an unconditional
`JIT_CACHE_GENERATION.fetch_add` in both `JitCache::put` and
`JitCache::put_osr`, so an address can never be re-issued without a generation
the reading thread has not yet seen.

* `clear_site_keyed_dispatch_memos()` — one function, clearing all eight
  site-keyed memos, so a memo added later cannot be flushed by one trigger and
  missed by the other. That split is exactly what left four unflushed.
* `flush_raw_entry_dispatch_caches()` now calls it from both its triggers.
* The flush was **moved earlier** in both helpers. It ran *after* the native
  fast paths that consult these very memos: in `jit_invoke_dispatch` it now
  runs immediately before `info_key` is built, and in
  `jit_invoke_virtual_mic` immediately before
  `try_jit_site_cached_native_dispatch` — a path that returns before the old
  flush site was ever reached.

Steady-state cost is unchanged: two thread-local compares, with every
`clear()` inside the changed-generation branch (marked `#[cold]`).

### The second defect (independent, and not the cause)

`build_class_annotation_array` passed `ClassId::new(0)` where its
Method/Field/Parameter siblings already passed
`java/lang/annotation/Annotation`, and all three `getAnnotationsByType` natives
passed it where the JDK declares `A[]`. A matrix probe diffed against the host
JDK over Class/Method/Field/Constructor/Parameter holders:

| accessor | CratonVM (before) | HotSpot |
|---|---|---|
| `Class.getDeclaredAnnotations()` | `[Ljava.lang.Object;` | `[Ljava.lang.annotation.Annotation;` |
| `Class.getAnnotations()` | `[Ljava.lang.Object;` | `[Ljava.lang.annotation.Annotation;` |
| `Class.getAnnotationsByType(Tag)` | `[Ljava.lang.Object;` | `[LTag;` |
| `Method.getAnnotationsByType(Tag)` | `[Ljava.lang.Object;` | `[LTag;` |

It was fixed first, on the hypothesis that the array's component class id —
the same header word a dispatch reads to name a receiver — explained the
`java.lang.Object.annotationType()` face. **It does not**: with only that fix
the crash rate was 9/20, statistically indistinguishable from the 5–9/20 of
the unmodified tree. Recorded here because the temptation to claim it is real,
and because the array types were genuinely wrong.

One divergence deliberately left alone: CratonVM returns `@Inherited`
annotations from `Class.getAnnotations()` in a different order than HotSpot.
`getAnnotations()` specifies no order, so this is not a defect.

## 5. Verification

All arms on one binary pair from the same tree, interleaved in both orders on
the same host, `DiscoveryProbe` on `OffsetDateTimeTest`, anomaly = SIGSEGV or
any exception:

| arm | runs | clean | SEGV | exception |
|---|---:|---:|---:|---:|
| pre-fix (annotation fix only) | 20 | 11 | 3 | 6 |
| pre-fix, block A1 | 10 | 6 | 1 | 3 |
| **fixed, block B1** | **20** | **20** | **0** | **0** |
| **fixed, block B2** | **20** | **20** | **0** | **0** |
| pre-fix, block A2 | 10 | 7 | 2 | 1 |

**0 / 60 on the fixed binary; 7 / 20 on the pre-fix binary in the same
session**, so the green is not a quiet host.

Full-class execution, fixed binary, 2 runs for 2:

```
@@RESULT …OffsetDateTimeTest found=488 started=488 ok=324 failed=0 aborted=164 skipped=0
```

**HotSpot control, same classpath, same JDK:**

```
@@RESULT …OffsetDateTimeTest found=488 started=488 ok=324 failed=0 aborted=164 skipped=0
```

Byte-identical. The 164 aborts are JUnit `Assumptions` self-skips baked into
the test, exactly like the three temporal siblings already recorded — the
class is now listed in `apps/hib-suite-runner/known-benign-aborts.tsv` with
those counts, so `run-hib.sh categorize` routes it to `passed.txt` and a
*different* abort profile still surfaces as a residual. That table was
untracked despite its own header requiring it to be force-added past the
`apps/` gitignore; it is now tracked and LF-pinned like its sibling.

Suite state on the merged tree:

* `apps/hib-suite-runner`, `passed.txt[0..240)`, real JDK, JIT on — **240/240
  PASS** on the fixed binary (two slices, 60 + 180).
* `cargo test -p cratonvm-native-builtins --lib` — 3267 passed, 0 failed.
* `cargo test -p cratonvm-vm --lib` — 2401 passed, **2 failed**:
  `native::jni::tests::jni_function_table_extended_to_234` and
  `jni_nio_slots_not_stub`. Both are pre-existing and unrelated — re-run on
  unmodified `dev` (`a0a648dfc`) they fail identically, same assertion, same
  folded address. They are the `/OPT:ICF` casualties already recorded in
  `../../native-call-funnel-per-call-floor-item2-20260805.md` §"`/OPT:ICF` —
  four tests, one cause": `jni_get_module` compiles to the same bytes as
  `jni_stub`, so the linker folds them and `assert_ne!` on the two addresses
  cannot hold.

Regression fixtures:

* `vm/tests/reflective_annotation_array_component.rs` — pins all ten
  accessors in both `--nojit` and JIT modes. **Verified RED on the pre-fix
  binary** (`left: "[Ljava.lang.Object;"`) and green on the fixed one.
* `vm/src/jit/helpers.rs::tests::a_jit_generation_change_clears_every_site_keyed_memo`
  — populates all eight memos, forces the generation to differ, and asserts
  every one is emptied. Non-vacuous by construction: it asserts each map is
  non-empty before the flush.

## 6. What this predicts elsewhere

The mechanism is not annotation-specific and not Hibernate-specific. Any
long-running workload that invalidates and republishes compiled code — the
condition is simply a non-trivial `jit: … cache generation N` in a crash
banner — could serve one call site's native or dispatch class to another.
Before attributing a future "wrong method called" / "NoSuchMethodError naming
a class that never declared the method" / "small integer used as a reference"
report to codegen, check whether it predates this fix.

Related, still open: `../../../known-issues/h2/bug-h2-classid0-stale-address-family.md`
is a *different* family (a collector reclaiming or relocating something still
referenced). This page is not a witness of it, for the two reasons the
original page already gave.
