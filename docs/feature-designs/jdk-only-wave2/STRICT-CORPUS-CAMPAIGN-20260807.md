# The strict-corpus campaign — 14 failures, worked in parallel, 2026-08-06/07

**Status: IN FLIGHT. Nothing here has been built or re-measured.** Every fix
named below is source-level, written by a lane that could not run `cargo` or the
VM. No arm of the corpus has been re-run since the fixes were written. Read
every "FIXED" in a lane record as *"fixed in source, unverified by execution"* —
several of the lane records say exactly that in their own headers.

This is the record of one campaign: the `--jdk-only` strict regression corpus
(`regression-suite/run.sh`, `JDKONLY_CLASSES`, 21 vectors) reported **14
failures**, and a parallel pool was staffed one lane per failure. The evidence
per defect lives in [`docs/known-issues/jdk-only/`](../../known-issues/jdk-only/),
one `L<n>-*.md` per lane. This file is the campaign-level view: the classified
breakdown, what each lane found, the two verdicts that had to be corrected, and
what still has to be measured.

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

That is the wave's central pattern confirmed for the fourth time (L8's retirement
note, 2026-08-05, said it first: "Zero were introduced by `--jdk-only`").

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
layer, `RJdkModule.java:57`:

```java
check(m.getLayer() == ModuleLayer.boot(), "module must be in the boot layer");
```

Note what *did* pass: `svc()` at `RJdkModule.java:46-48` already ran
`ModuleLayer.boot().findModule(MODULE)` and asserted `isPresent()`. So the boot
layer knows the module and `Module.getLayer()` does not answer with it — the
failure is narrower than "module path unsupported". Lane L9 owns it; no record
exists in `docs/known-issues/jdk-only/` yet.

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
Lane L10 measured it directly: a live child is in `children()` and
`descendants()` on the very first snapshot, 5/5, with no fork-visibility delay to
tolerate; the fast-exiting one is already gone.

L10 rewrote the vector so the process-tree section asserts against a *live*
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
Rust (or Java, for L10) that has not been compiled. "Out-of-file" means the lane
did not own the file the fix has to land in and handed a patch to the
orchestrator; those patches are the most likely thing to be missing from a
merged tree, and several lanes name the exact observation that separates "the
patch was not applied" from "the diagnosis is wrong".

| lane record | vector | root cause | state | out-of-file patches |
| --- | --- | --- | --- | --- |
| [L1](../../known-issues/jdk-only/L1-reflect-setaccessible-invoke.md) | `RJdkReflect` | `Method.invoke` has no caller step, so nestmate private invocation is refused; `check_access` takes no `ctx` and cannot ask who the caller is | fix written | 1 (`lang_class.rs`, the `native_method_invoke` call site) |
| [L2](../../known-issues/jdk-only/L2-methodtypeform-lambdaforms-null.md) | `RJdkHandles` | a fabricated `MethodTypeForm` declares itself *basic* and carries the null `lambdaForms`/`methodHandles` caches of a *non-basic* form; the JDK's accessors have no null check | fix written | 1 (`vm_exec.rs` `check_override`, without which the new `asVarargsCollector`/`asFixedArity` shims are never dispatched) |
| [L3](../../known-issues/jdk-only/L3-definehiddenclass-returns-null.md) | `RJdkHidden` **and** `RJdkStrict` | a phase-54 placeholder `defineHiddenClass` ignores its arguments and never writes slot 0 (`lookupClass`), shadowing the real implementation | **diagnosed, no code changed** — the whole defect is outside the lane's files | 3 (patch 1 required and fixes both classes; patch 2 required for `RJdkHidden`'s name assertion; patch 3 hardening) |
| [L4](../../known-issues/jdk-only/L4-jmx-iterator-remove-default-method.md) | `RJdkJmx` | **two** defects, one per arm: `--real-jdk` has no `getObjectName()` on the interface-stamped MXBeans; `--jdk-only` gets a real `Arrays$ArrayItr` that has no `remove()` | `--real-jdk` arm fix written; `--jdk-only` arm diagnosed only | the strict half is a `native-collections` change the lane does not own |
| [L5](../../known-issues/jdk-only/L5-files-copy-alreadyexists-and-filevisitoption-dispatch.md) | `RJdkNio` | `Files.copy` never reads its `CopyOption[]` and overwrites silently; and a `FileVisitOption[]` is asked `isEmpty()`, logging a `NoSuchMethodError` on every `Files.walk` | helper landed in `native-io`; the live fix is out-of-file | 2 required + 1 hygiene (`phases_late/nio_file.rs` ×2, `native-io/src/lib.rs`) |
| [L6](../../known-issues/jdk-only/L6-so-rcvbuf-getoption.md) | `RJdkNet` | `Net.getIntOption0` answered a remembered request, never the OS; and the `SOL_SOCKET`/`SO_KEEPALIVE` constants it compared against were the Linux numbering | fix written, self-contained | none |
| [L7](../../known-issues/jdk-only/L7-adler32-missing-natives.md) | `RJdkJni` | `java.util.zip.Adler32` had **no** real-JDK natives at all; the synthetic model used disjoint descriptors, so every mode-agnostic census reported the class as covered | fix written, self-contained | none |
| [L8](../../known-issues/jdk-only/L8-securerandom-provider.md) | `RJdkSecurity` | every construction route stamped `algorithm` and never `provider`, so `getProvider()` was always null; and `getInstance(String)` fabricated a PRNG for any string | fix written, self-contained | none |
| L9 — **no record yet** | `RJdkModule` | `Module.getLayer()` does not answer the boot layer, though `ModuleLayer.boot().findModule()` finds the module | in flight | unknown |
| [L10](../../known-issues/jdk-only/L10-rjdkprocess-vector-overassertion.md) | `RJdkProcess` | **the vector, not the VM** — a process-tree snapshot asserted against a child that had already exited | vector fixed and measured 26/26 green; **no VM code changed** | none |
| [L11](../../known-issues/jdk-only/L11-altmetafactory-marker-interfaces.md) | `RJdkLambdas` | `altMetafactory`'s `FLAG_MARKERS` block is parsed by neither producer, has no place in `LambdaCallSite`, and is never consulted by the type test — three gaps on one axis | fix written | 4 (`typecheck.rs`, `native-api/src/registry.rs`, `vm_exec.rs`, `lang_invoke.rs`) |

**Twelve of the fourteen failing classes are named above.** The identity of the
other two is not recorded anywhere in this worktree — **UNVERIFIED**; whoever
holds the corpus transcript should fill them in rather than guess. The 21
scheduled vectors are `run.sh:72`.

### Two structural notes that fall out of the table

* **`RJdkJmx` is the counter-example to "the two arms fail identically".** L4
  diffed the captured logs instead of trusting the brief and found the arms fail
  at *different lines, for different reasons* — `--real-jdk` gets two whole test
  methods further. A brief that says "fails in both arms" is a claim about a
  transcript nobody diffed.
* **A fixed vector does not mean a fixed class.** L5 notes `RJdkNio` dies at
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
