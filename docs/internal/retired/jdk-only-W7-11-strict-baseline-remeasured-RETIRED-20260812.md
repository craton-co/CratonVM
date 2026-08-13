> **RETIRED 2026-08-12 — moved out of `docs/known-issues/jdk-only/`.**
>
> A measurement record. It closed at **68 passed / 0 failed** on the day it was written, and all four defects it named are fixed in the tree:
>
> * `RJdkForkJoin` — the hard-coded JDK-21-era `$DefaultCommonPoolForkJoinWorkerThreadFactory` is gone; `46bb0ad2e` resolves the factory from the image's own public static field (`common_factory_from_image`, `native-builtins/src/phases_late/concurrent.rs:8530`), with both spellings pinned by `t19_k3_safe_factory_class_name_accepts_quarkus` (`:8720-8731`).
> * `RChmKeySetView` — `NoClassDefFoundError: java/util/HashMap$KeyItr` closed by `8b4443fc6` then `beb8acee7`; receiver readmitted at `native-api/src/no_image_receiver.rs:148` with the reasoning at `:104-110`.
> * `RJdkJmx` / `RReflect` / `RJdkReflect` — `5266bf8c7` routed the annotation carrier through the VM-internal door (`classloading/src/class_manager.rs:11342`, `:11362`).
> * `RJdkHandles` — `87ab40daf` mints the ten combinator carriers as VM-internal (`native-builtins/src/lang_invoke.rs:6268`, `:10315`).
>
> **Its one quiet row was adjudicated separately, per the counter-rule.** The `cratonvm/internal/Unmodifiable*` link it proposed was a diagnostic conjecture explicitly filed as *"plausible and not yet proven"*; the four closures above name four different, unrelated root causes, so the conjecture was never the cause. The surface it conjectured about has also shrunk — only `cratonvm/internal/UnmodifiableList` survives in `native-api/src/no_image_receiver.rs:433`. Nothing to carry forward.
>
> The 68/0 was taken on a binary built 2026-08-11 and HEAD is well past it. That is ordinary drift, not an unfinished item; the strict baseline is re-taken by the suite, not by this record.
>
> Previous location: `docs/known-issues/jdk-only/W7-11-strict-baseline-remeasured.md`.
> Audit that moved it: `docs/known-issues/jdk-only/RETIREMENT-20260812.md`.

# The strict baseline is 62/6, not 53/1 — and the six are pre-existing

**Status: MEASURED 2026-08-11, then CLOSED the same day at 68/0.** Not a defect
record: a correction to the number every other record in this directory is read
against, plus the identities of six strict-mode failures that had never been
attributed.

> ## Closed — the corpus is green in both modes
>
> All six were fixed the same day, each by a separate lane, and re-measured on
> one binary (`dev` after the wave, built 20:54):
>
> ```
> --jdk-only   REGRESSION SUITE: 68 passed, 0 failed
> --real-jdk   REGRESSION SUITE: 41 passed, 0 failed
> ```
>
> | vector | was | root cause |
> |---|---|---|
> | `RChmKeySetView` | `NoClassDefFoundError: java/util/HashMap$KeyItr` | a snapshot iterator fabricating a name no image declares, and propagating the refusal with a bare `?` |
> | `RJdkJmx` / `RReflect` / `RJdkReflect` | `AnnotationProxy`, and two `AssertionError`s | one refusal wearing two faces — the array path used `?`, the single-annotation paths swallowed it into a `null` return |
> | `RJdkHandles` | `NoClassDefFoundError: __mh_insert_wrapper__` | ten combinator carriers minted through the compatibility door instead of the VM-internal one |
> | `RJdkForkJoin` | `…$DefaultCommonPoolForkJoinWorkerThreadFactory` | a JDK-21-era nested class name hard-coded; JDK 25 declares only `$DefaultForkJoinWorkerThreadFactory` |
>
> Criterion 6 of the contract — "strict corpus green" — is met. The README's own
> warning about it stands and is worth keeping: it is *"not a formality to tick
> after the list is done; it is where the defects are."* Six of them were.
>
> **What made this measurable was the control, not the fix.** Two intermediate
> builds failed at the link step (`failed to remove file … Access denied`,
> because a probe run held the binary) while the shell reported success, so a
> full suite run was taken against a four-build-old binary and read as "the
> fixes did not take". The tell was that the six failures were *byte-identical*
> to the control. Compare the binary's mtime against the merge commit times
> before believing any suite result, and check cargo's own exit code rather than
> a trailing command's.

## What the directory said

The README's headline note, dated 2026-08-07, says the strict corpus was built,
run three times ABBA-interleaved, and came out at **53 passed, 1 failed**, the
one failure being `RMapGcStress` — "dev's own, not this campaign's". Every
"discharged at the vector level" claim in this directory rests on that line.

## What it measures today

Same command, `JDK=<jdk-25> CRATONVM_ARGS="--jdk-only" bash regression-suite/run.sh`,
on `dev` at `c72c4a520`:

```
REGRESSION SUITE: 62 passed, 6 failed
  ( failed: RReflect RChmKeySetView RJdkHandles RJdkReflect RJdkForkJoin RJdkJmx )
```

`RMapGcStress` **passes**. The suite is also larger than it was — 68 vectors
against 54 — because `RJdkViews`, `RPriorityQueueGc` and `RTreeRangeGc` are
scheduled now and other lanes have added vectors since. So the two totals are
not comparable directly, and the interesting question is not the count but
whether the six are new.

## They are not new. The control says so.

`dev` was at `7d4d545e0` before this wave's sixteen branches merged. That commit
was checked out into a separate worktree, built, and run against the **same**
suite build on the **same** host. All six fail there too, with byte-identical
identities:

| vector | failure | control `7d4d545e0` | merged `c72c4a520` |
|---|---|---|---|
| `RChmKeySetView` | `NoClassDefFoundError: java/util/HashMap$KeyItr` | FAIL | FAIL |
| `RJdkJmx` | `NoClassDefFoundError: java/lang/annotation/AnnotationProxy` | FAIL | FAIL |
| `RJdkHandles` | `NoClassDefFoundError: __mh_insert_wrapper__` | FAIL | FAIL |
| `RReflect` | `AssertionError: getAnnotation present` | FAIL | FAIL |
| `RJdkReflect` | `AssertionError` | FAIL | FAIL |
| `RJdkForkJoin` | `NoClassDefFoundError: ForkJoinPool$DefaultCommonPoolForkJoinWorkerThreadFactory` | FAIL | FAIL |

All six **pass** under `--real-jdk` on both binaries, so this is strict-mode
behaviour, not a compatibility defect, and `Compatible` mode is unchanged by the
wave — which is what contract §5/§10 require.

Two of the six names are not JDK classes at all. `java/lang/annotation/AnnotationProxy`
and `__mh_insert_wrapper__` are VM-minted, so those two are strict declining to
fabricate a class that nothing registers — the shape §1.4 and W2-1 are about,
not a missing-class problem.

## Why this went unnoticed

The 2026-08-07 run was real and its number was true when taken. What made it
durable-looking is that nothing re-took it: the figure was quoted forward for
four days, across a period in which the suite gained vectors and `dev` gained
commits from other lanes. A count with no re-measurement is a claim about the
day it was taken, and this directory's own standing advice — take the census
from the workload you care about rather than sizing from a stale figure —
applies to its own headline.

The corollary is the useful part: **`RMapGcStress`, the one failure the README
dismissed as not this campaign's, is fixed, and six failures the README does not
mention are live.** A pass/fail total hid both.

## What each of the six needs

Three clusters, being worked separately:

* **collections / ForkJoin** — `HashMap$KeyItr` and the common-pool worker
  thread factory. Both runs open with strict refusing
  `cratonvm/internal/Unmodifiable{Map,Itr,ListItr,EntrySet,EntryItr,MapEntry}`,
  which is W2-1's "the stream stack is SPLIT" exactly. The link is plausible and
  **not yet proven**: a slash-form `NoClassDefFoundError` is not a `<clinit>`
  verdict, and the refusal message's "the natives bound to it are unreachable"
  is a statement about registration, not about whether the workload needs the
  class.
* **annotations** — `AnnotationProxy`, plus the two `getAnnotation present`
  assertions. One cause, three vectors, most likely.
* **MethodHandles** — `__mh_insert_wrapper__`, a combinator carrier.

## How to re-take this

```sh
JDK="<jdk-25-home>" CRATONVM_ARGS="--jdk-only" bash regression-suite/run.sh
```

`run.sh` gives every CratonVM invocation a `--java-home`; a hand-run that omits
it measures the host's default JDK instead of the JDK 25 image and has inverted
a per-mode verdict before. To attribute a failure, build the previous commit
into its own worktree and run the **same** `regression-suite/build` output
through both binaries — the suite compiles once, so the class files are held
constant and only the VM varies.
