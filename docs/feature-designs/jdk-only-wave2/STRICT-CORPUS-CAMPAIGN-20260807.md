# The strict-corpus campaign — 14 failures, worked in parallel, 2026-08-06/07

**Status: IN FLIGHT. Nothing here has been built or re-measured.** Every fix
named below is source-level, written by a lane that could not run `cargo` or the
VM. No arm of the corpus has been re-run since the fixes were written. Read
every "FIXED" in a lane record as *"fixed in source, unverified by execution"* —
several of the lane records say exactly that in their own headers.

This is the record of one campaign: the `--jdk-only` strict regression corpus
(`regression-suite/run.sh`, `JDKONLY_CLASSES`, 21 vectors) reported **14
failures**, and a parallel pool was staffed one lane per failure — **`SC-1` …
`SC-16`** here. The evidence per defect lives in
[`docs/known-issues/jdk-only/`](../../known-issues/jdk-only/), one file per lane,
still named `L<n>-*.md`; *Lane label reconciliation* below maps every one. This
file is the campaign-level view: the classified breakdown, what each lane found,
the two verdicts that had to be corrected, and what still has to be measured.

The execution plan for the *earlier* wave-2 lanes (L1–L12, file-ownership map,
conflict matrix, verification protocol) is [`README.md`](README.md) in this
directory, and it stays the guide.

> **This campaign's lanes are `SC-<n>` (strict corpus). Use that label, never a
> bare `L<n>`.** Two L-numberings are live in this repository and they collide
> head-on: `docs/feature-designs/jdk-only-wave2/L3-scanner-membername-residual.md`
> is wave-2 lane L3 (Scanner/MemberName layout rows, landed 2026-08-05), while
> `docs/known-issues/jdk-only/L3-definehiddenclass-returns-null.md` is **SC-3**
> (`Lookup.defineHiddenClass`). They are unrelated.
>
> The `docs/known-issues/jdk-only/L*.md` **filenames stay as they are** — several
> lanes are still writing them, and renaming a file out from under a live lane is
> worse than the ambiguity. The mapping table below is the reconciliation: cite
> the **full path** and the `SC-<n>` label together, every time.

---

## The headline finding

**Of the 14 strict-corpus failures, 12 fail in `--real-jdk` as well, and ZERO
are strict-only.**

`--jdk-only` is not broken in the sense the mode's name invites — "strict policy
rejects something legitimate". It is *executing* code paths that `Compatible`
mode was papering over with synthetic stubs, and the code underneath those paths
is wrong in both modes. Every lane that had a HotSpot control and both CratonVM
arms said the same thing in the same words: the two CratonVM arms produce a
**byte-identical** trace, which is the tell that nothing was refused. The
remaining work is ordinary subsystem bug-fixing that fixes both modes; it is not
`--jdk-only` contract work.

That is the wave's central pattern confirmed for the fourth time (wave-2 lane
L8's retirement note, 2026-08-05, said it first: "Zero were introduced by
`--jdk-only`").

### The shared disease shape

Named independently by four lanes, in four subsystems:

**A synthetic stub or placeholder shadowing real JDK bytecode that already
works.** Registration is last-write-wins on the triple, so a placeholder
registered *later* in the boot sequence silently outranks a correct
implementation registered earlier — with no exception, no warning, and no census
row that looks wrong.

| lane | the shadowing row | what it hid |
| --- | --- | --- |
| SC-3 | `lang_invoke.rs`'s `defineHiddenClass` placeholder (phase 54) | `lookup_define.rs::lk_define_hidden_class_full`, the real WP2.3-B implementation |
| SC-5 | `native-builtins`' `Files.copy` (phase 57) | `native-io`'s `native_files_copy` — and *both* ignored `CopyOption[]` |
| SC-8 | — | `crypto_impl.rs`'s `SecureRandom` rows win only in synthetic-jdk mode; the live registrar wrote `algorithm` and never `provider` |
| SC-2 | — | a fabricated `MethodTypeForm` that never ran `<init>`, so the JDK's own null-check-free cache accessors NPE'd |
| SC-12 | `ForkJoinTask.fork()`, a deliberately lazy `Bridge` | the real `invokeAll` bytecode, which forks and then blocks on an `awaitDone` no CratonVM worker will ever satisfy |

SC-3's is the cleanest instance: one deletion of ten lines fixes two regression
classes at once. SC-12's is the shape's worst outcome — a **hang** rather than an
exception, because the stub and the real bytecode disagree about who runs the
work.

---

## The two corrections

### 1. `RJdkModule` was wrongly written off. It is a real CratonVM defect.

The orchestrator's ad-hoc HotSpot arm passed the wrong module name —
`cratonvm.regression.jdkonly`. The module in this tree is
**`cratonvm.jdkonly.svc`** (`regression-suite/modules/cratonvm.jdkonly.svc/module-info.java`,
and `run.sh:58` `JDKONLY_MODULE="cratonvm.jdkonly.svc"`). With the correct
invocation HotSpot 25 passes:

```sh
cd regression-suite
"/c/Program Files/Microsoft/jdk-25.0.3.9-hotspot/bin/java" \
    --module-path build-modules --add-modules cratonvm.jdkonly.svc \
    -cp build RJdkModule
# -> PASS RJdkModule (44 checks), rc=0
```

CratonVM fails in **both** modes at the first check that asks anything about the
layer, `RJdkModule.java:57` — the **4th** check of the first section, so nothing
about module support is exercised at all and no `CK RJdkModule` line is emitted:

```java
check(m.getLayer() == ModuleLayer.boot(), "module must be in the boot layer");
```

Lane **SC-9** has since filed
[`../../known-issues/jdk-only/L9-module-not-in-boot-layer.md`](../../known-issues/jdk-only/L9-module-not-in-boot-layer.md),
which records the same harness error independently and names the root cause:
`--module-path` / `--add-modules` were **parsed and then read by nobody**. The
resolution layer landed in `classloading/src/module.rs`; two wiring patches are
out-of-file and were not applied. Its own note is worth carrying — the captured
`scratchpad/rjdk/RJdkModule.hs` still holds the stale invocation and its
`FindException: Module cratonvm.regression.jdkonly not found`. **A stale capture
outlives the correction that retired it; delete it or label it, or it will be
re-read as evidence.**

**The generalisable lesson: a "HotSpot fails it too" verdict is only as good as
the invocation.** Measuring against a broken oracle produced a wrong
classification that would have suppressed a real defect for the whole campaign.
Run the oracle the way the suite runs it — `run.sh:130` and `:143` build the
`--module-path … --add-modules …` argument for both arms from one variable
precisely so the two cannot drift.

### 2. `RJdkProcess` is a genuine vector bug — HotSpot 25 really does fail it

Same class of question, opposite answer, which is why the first correction is a
lesson about *method* and not about "always distrust a HotSpot-fails verdict".

`AssertionError: the child must appear in our children()`
(`RJdkProcess.childProcess:132`). The vector spawned `cmd.exe /c exit 3` — a
child chosen precisely because it exits immediately, since it is the exit-code
subject — and then took three separate `parent()` / `children()` /
`descendants()` snapshots of the **live OS process table**. Three coin flips.
Lane **SC-10** measured it directly: a live child is in `children()` and
`descendants()` on the very first snapshot, 5/5, with no fork-visibility delay to
tolerate; the fast-exiting one is already gone.

SC-10 rewrote the vector so the process-tree section asserts against a *live*
child (`ping -n 30` / `sleep 30`, with `isAlive()` guards on both sides of the
section) and the exit-code section against the exiting child. **Nothing was
weakened or deleted**: 46 → 53 checks, 7 assertions added, none removed. Measured
**26/26 green**, including 18 runs under self-inflicted load; the pre-fix vector
failed 3/3.

**Every pre-fix `RJdkProcess` result is void.** A failure at
`RJdkProcess.java:132` or `:134` carries zero information about CratonVM. The
class has to be re-measured from scratch, and post-fix a failure in the tree
section *is* real — the child is provably alive for the full 10 s window.

Record: [`../../known-issues/jdk-only/L10-rjdkprocess-vector-overassertion.md`](../../known-issues/jdk-only/L10-rjdkprocess-vector-overassertion.md).

---

## Per-lane state

Every row is **source-level and unverified**. "Fix written" means a lane wrote
Rust (or Java, for SC-10) that has not been compiled. "Out-of-file" means the lane
did not own the file the fix has to land in and handed a patch to the
orchestrator; those patches are the most likely thing to be missing from a
merged tree, and several lanes name the exact observation that separates "the
patch was not applied" from "the diagnosis is wrong".

| lane | vector | root cause | state | out-of-file patches |
| --- | --- | --- | --- | --- |
| **SC-1** | `RJdkReflect` | `Method.invoke` has no caller step, so nestmate private invocation is refused; `check_access` takes no `ctx` and cannot ask who the caller is | fix written | 1 (`lang_class.rs`, the `native_method_invoke` call site) |
| **SC-2** | `RJdkHandles` | a fabricated `MethodTypeForm` declares itself *basic* and carries the null `lambdaForms`/`methodHandles` caches of a *non-basic* form; the JDK's accessors have no null check | fix written | 1 (`vm_exec.rs` `check_override`, without which the new `asVarargsCollector`/`asFixedArity` shims are never dispatched) |
| **SC-3** | `RJdkHidden` **and** `RJdkStrict` | a phase-54 placeholder `defineHiddenClass` ignores its arguments and never writes slot 0 (`lookupClass`), shadowing the real implementation | **diagnosed, no code changed** — the whole defect is outside the lane's files | 3 (patch 1 required and fixes both classes; patch 2 required for `RJdkHidden`'s name assertion; patch 3 hardening) |
| **SC-4** | `RJdkJmx` | **two** defects, one per arm: `--real-jdk` has no `getObjectName()` on the interface-stamped MXBeans; `--jdk-only` gets a real `Arrays$ArrayItr` that has no `remove()` | `--real-jdk` arm fix written; `--jdk-only` arm diagnosed only | the strict half is a `native-collections` change the lane does not own |
| **SC-5** | `RJdkNio` | `Files.copy` never reads its `CopyOption[]` and overwrites silently; and a `FileVisitOption[]` is asked `isEmpty()`, logging a `NoSuchMethodError` on every `Files.walk` | helper landed in `native-io`; the live fix is out-of-file | 2 required + 1 hygiene (`phases_late/nio_file.rs` ×2, `native-io/src/lib.rs`) |
| **SC-6** | `RJdkNet` | `Net.getIntOption0` answered a remembered request, never the OS; and the `SOL_SOCKET`/`SO_KEEPALIVE` constants it compared against were the Linux numbering | fix written, self-contained | none |
| **SC-7** | `RJdkJni` | `java.util.zip.Adler32` had **no** real-JDK natives at all; the synthetic model used disjoint descriptors, so every mode-agnostic census reported the class as covered | fix written, self-contained | none |
| **SC-8** | `RJdkSecurity` | every construction route stamped `algorithm` and never `provider`, so `getProvider()` was always null; and `getInstance(String)` fabricated a PRNG for any string | fix written, self-contained | none |
| **SC-9** | `RJdkModule` | `--module-path` / `--add-modules` were **parsed and then read by nobody**; the boot layer therefore does not own the module, so `getLayer()` fails on check 4 of 44 | PARTIAL — the resolution layer landed in `classloading/src/module.rs` | 2 wiring patches, not applied |
| **SC-10** | `RJdkProcess` | **the vector, not the VM** — a process-tree snapshot asserted against a child that had already exited | vector fixed and measured 26/26 green; **no VM code changed** | none |
| **SC-11** | `RJdkLambdas` | `altMetafactory`'s `FLAG_MARKERS` block is parsed by neither producer, has no place in `LambdaCallSite`, and is never consulted by the type test — three gaps on one axis | fix written | 4 (`typecheck.rs`, `native-api/src/registry.rs`, `vm_exec.rs`, `lang_invoke.rs`) |
| **SC-12** | `RJdkForkJoin` | a lazy `fork()` `Bridge` marks the task queued and returns, but the real `invokeAll` bytecode then blocks in `awaitDone` waiting for a worker that does not exist — **rc=124, a true hang** | fix written; **removes the hang, does not pass the class** — the `CountedCompleter` section is then expected to fail loudly and fast | see the record |
| **SC-16** — record pending, will be `docs/known-issues/jdk-only/L16-classnotfound-vs-noclassdeffound-shapes.md` | `RJdkFailure` | wrong error **shape**: a `NoClassDefFoundError` (caused by `ClassNotFoundException`) escapes to `main` where the vector's `catch (ClassNotFoundException)` cannot catch it — `Error` is not an `Exception` | in flight | unknown |

**All fourteen failures are now accounted for.** The suite line, dev tip
`84b85c624`:

```
REGRESSION SUITE: 37 passed, 14 failed ( failed: RJdkStrict RJdkLambdas RJdkHandles
RJdkReflect RJdkHidden RJdkModule RJdkForkJoin RJdkNio RJdkNet RJdkProcess
RJdkSecurity RJdkJmx RJdkJni RJdkFailure )
```

Thirteen vectors for fourteen failures, because SC-3's one root cause accounts
for both `RJdkHidden` and `RJdkStrict`. The 21 scheduled vectors are `run.sh:72`.

### The two that were unidentified until 2026-08-07

* **`RJdkForkJoin` (SC-12) — rc=124, a genuine hang**, confirmed against a 300 s
  budget with **zero** checkpoint output, where HotSpot 25 finishes in seconds
  with `PASS RJdkForkJoin (26 checks)`. The watchdog
  (`CRATONVM_DEFAULT_WATCHDOG_SEC=45`) dumps `1 thread(s) dumped`: main recursed
  **inline** through `compute -> invokeAll -> doExec -> exec` about eight times
  and then parked at `ForkJoinTask.awaitDone(FJP,IZJ)I pc=218`. One thread, all
  of it on the calling thread, is the whole diagnosis — there is no worker to
  wake it. A follow-on lane is working the `CountedCompleter` case behind an env
  gate. Record:
  [`../../known-issues/jdk-only/L12-forkjoinpool-no-workers-awaitdone-hang.md`](../../known-issues/jdk-only/L12-forkjoinpool-no-workers-awaitdone-hang.md).
  **An rc=124 in this corpus is a hang, not a slow host** — read the watchdog
  dump, not the wall clock.
* **`RJdkFailure` (SC-16) — a wrong error shape.** `NoClassDefFoundError:
  com/cratonvm/absent/NoSuchClass20260731`, caused by `ClassNotFoundException`,
  escapes to main at `RJdkFailure.missingClass:157`. This is a **negative test**:
  it provokes a missing class and expects to catch one specific type, and
  `catch (ClassNotFoundException)` does not catch an `Error`. Not a missing
  feature — the class *is* correctly absent; we raise the wrong member of the
  pair.

### One defect the strict corpus structurally cannot see

Lane **SC-14**
([`../../known-issues/jdk-only/L14-serviceloader-instance-caching.md`](../../known-issues/jdk-only/L14-serviceloader-instance-caching.md))
found `ServiceLoader`'s instance cache allocated, cleared on `reload()` and
**never read** — and `RJdkServices` is *not* in the 14, because it fails in the
**default `--real-jdk`** arm and passes under `--jdk-only`. That inversion is the
headline finding's mirror image and sharpens it: the strict corpus is not merely
free of strict-only failures, it is **blind to a Compatible-only one**. Run both
arms, always; a green strict corpus is not a statement about `--real-jdk`.

Lane **SC-15**
([`../../known-issues/jdk-only/L15-nestmate-access-field-and-constructor.md`](../../known-issues/jdk-only/L15-nestmate-access-field-and-constructor.md))
has no vector of its own: it is SC-1's named residual — the field and
constructor reflection paths carry the same caller-step gap `Method.invoke` had.

## Lane label reconciliation

`SC-<n>` is this campaign. The filenames keep their `L<n>` prefix; the third
column is the wave-2 lane that shares the number and is **unrelated**.

| label | record (do not edit — lanes are still writing these) | vector / subsystem | collides with wave-2 lane |
| --- | --- | --- | --- |
| SC-1 | [`known-issues/jdk-only/L1-reflect-setaccessible-invoke.md`](../../known-issues/jdk-only/L1-reflect-setaccessible-invoke.md) | `RJdkReflect` — reflection caller step | L1, classloader side table |
| SC-2 | [`…/L2-methodtypeform-lambdaforms-null.md`](../../known-issues/jdk-only/L2-methodtypeform-lambdaforms-null.md) | `RJdkHandles` — `java.lang.invoke` | L2, `native_map_init` by-name |
| SC-3 | [`…/L3-definehiddenclass-returns-null.md`](../../known-issues/jdk-only/L3-definehiddenclass-returns-null.md) | `RJdkHidden` + `RJdkStrict` — hidden classes | L3, Scanner/MemberName residual |
| SC-4 | [`…/L4-jmx-iterator-remove-default-method.md`](../../known-issues/jdk-only/L4-jmx-iterator-remove-default-method.md) | `RJdkJmx` — JMX + iterators | L4, overlay-detector blind spots |
| SC-5 | [`…/L5-files-copy-alreadyexists-and-filevisitoption-dispatch.md`](../../known-issues/jdk-only/L5-files-copy-alreadyexists-and-filevisitoption-dispatch.md) | `RJdkNio` — `java.nio.file` | L5, `register_with_kind` migration |
| SC-6 | [`…/L6-so-rcvbuf-getoption.md`](../../known-issues/jdk-only/L6-so-rcvbuf-getoption.md) | `RJdkNet` — socket options | L6, unadjudicated-`Bridge` ratchet |
| SC-7 | [`…/L7-adler32-missing-natives.md`](../../known-issues/jdk-only/L7-adler32-missing-natives.md) | `RJdkJni` — `java.util.zip` | L7, `ensure_synthetic_class` migration |
| SC-8 | [`…/L8-securerandom-provider.md`](../../known-issues/jdk-only/L8-securerandom-provider.md) | `RJdkSecurity` — JCA | L8, "strict corpus green" (retired) |
| SC-9 | [`…/L9-module-not-in-boot-layer.md`](../../known-issues/jdk-only/L9-module-not-in-boot-layer.md) | `RJdkModule` — JPMS | L9, the RKC16N.6 `String` blocker |
| SC-10 | [`…/L10-rjdkprocess-vector-overassertion.md`](../../known-issues/jdk-only/L10-rjdkprocess-vector-overassertion.md) | `RJdkProcess` — **the vector itself** | L10, the `ThreadPoolExecutor` blocker |
| SC-11 | [`…/L11-altmetafactory-marker-interfaces.md`](../../known-issues/jdk-only/L11-altmetafactory-marker-interfaces.md) | `RJdkLambdas` — `altMetafactory` markers | L11, delete the hard-coded lists |
| SC-12 | [`…/L12-forkjoinpool-no-workers-awaitdone-hang.md`](../../known-issues/jdk-only/L12-forkjoinpool-no-workers-awaitdone-hang.md) | `RJdkForkJoin` — fork/join, a hang | L12, item 11 residuals |
| SC-14 | [`…/L14-serviceloader-instance-caching.md`](../../known-issues/jdk-only/L14-serviceloader-instance-caching.md) | `RJdkServices` — **fails in `--real-jdk` only**, not in the 14 | none (wave 2 has no L13+) |
| SC-15 | [`…/L15-nestmate-access-field-and-constructor.md`](../../known-issues/jdk-only/L15-nestmate-access-field-and-constructor.md) | SC-1's residual — field/constructor caller step | none |
| SC-16 | pending: `…/L16-classnotfound-vs-noclassdeffound-shapes.md` | `RJdkFailure` — error shape | none |

No SC-13 or SC-17 record exists in this worktree; if one appears, extend this
table rather than renumbering.

### Two structural notes that fall out of the table

* **`RJdkJmx` is the counter-example to "the two arms fail identically".** SC-4
  diffed the captured logs instead of trusting the brief and found the arms fail
  at *different lines, for different reasons* — `--real-jdk` gets two whole test
  methods further. A brief that says "fails in both arms" is a claim about a
  transcript nobody diffed.
* **A fixed vector does not mean a fixed class.** SC-5 notes `RJdkNio` dies at
  check ~14 of 78, so `raf+map`, `buffers`, `asyncClose` and most of `filesApi`
  have **never executed** under CratonVM. Expect further findings on the next
  run; do not read "these two defects are fixed" as "the class passes".

---

## The measurement caveat that must not be lost

`scripts/jdk-only-strict-probes.sh` — the named gate for contract criterion 6 —
is down to **2 baselined divergences**
(`scripts/baselines/jdk-only-strict-corpus-25-linux.txt`, both
`JdkOnlyPlatformProbe/*/vthreads`, and note they are one in *each* arm, so even
those two are not strict-only). This corpus, in the same week, showed **14
failures**.

Both numbers are honest. They measure different things: the probe gate scores
five hand-written breadth probes section-by-section against a HotSpot control,
and the corpus runs 21 hostile vectors that assert JDK contracts check by check.
**Reading criterion 6 off the probe baseline alone badly overstates where the
mode is**, and the gap is not a rounding error — it is 2 against 14.

Say this wherever criterion 6 is discussed. It is the same trap as
*"a gate that measures a fraction reads as good news"*: a green ratchet over a
narrow instrument is evidence about the instrument's reach, not about the mode.

---

## What still has to be measured

Nothing below has been done. In order:

1. **Build.** No lane in this campaign could run `cargo`. Nothing is compiled.
2. **Apply the out-of-file patches, then verify they are in the merged tree.**
   Five lanes handed patches to files they did not own. L2 and L3 each name the
   exact observation that distinguishes an unapplied patch from a wrong
   diagnosis; use them rather than re-deriving.
3. **Re-run the whole corpus in three arms** — `--jdk-only`, `--real-jdk`, and
   HotSpot 25 — with the module path passed the way `run.sh` passes it. A
   divergence present in both CratonVM arms is not a strict-mode defect.
4. **Re-measure `RJdkProcess` from zero.** Every result before 2026-08-06 is
   void.
5. **Re-freeze the baselines the lanes named.** They disagree about which move,
   and each lane says why in its own *Baselines* section: L2 (`bridge-ratchet`,
   +2), L3 (`kind-map` collapses a duplicate pair to one row; bridge counts fall
   by 1), L4 (`bridge_without_acc_native` 9528 → 9536), L5/L7/L8 (none).
   Re-take the census on a JDK-bearing host; do not hand-edit.
6. **Identify the two unnamed failures**, and file them like the rest.
7. **Only then** restate criterion 6 — with both numbers, the probe gate's and
   the corpus's, side by side.
