# `java.lang.ProcessImpl` — nine unregistered Windows natives, and a tenth under a descriptor the image never declared

<!-- merge: both sides kept; the lane's finding and the reconciliation's commit attribution are complementary -->
**Status: RETIRED 2026-08-12 (W7-46).** Every row of the census below is
registered in `native-io/src/process.rs::register_process_natives`, under the
corrected `create` descriptor, inside the `#[cfg(windows)]` block, and each one
states `NativeKind::Bridge` at its own call site through `register_with_kind`
rather than inheriting the ambient category — so the ambient-`NativeKind`
hazard this campaign has hit elsewhere cannot reach them. Re-verified two ways
on 2026-08-12, not inferred:

* **Missing vs merely overwritten.** Every `java/lang/ProcessImpl` and
  `java/lang/ProcessHandleImpl` triple was grepped across the whole tree for a
  second registrar, because `register()` is last-write-wins and an overwritten
  native is indistinguishable from a missing one from the outside. There is
  exactly one other `ProcessImpl` registration anywhere —
  `init()V` in `native-builtins/src/lib.rs` — and it is a different method
  name. **No ProcessImpl or ProcessHandleImpl native is shadowed by a second
  registrar.** The ten were genuinely missing, and they are genuinely there now.
* **The out-of-file patch below has LANDED**, as W7-10, not as written here:
  `children()`, `descendants()`, `parent()` and `info()` on the
  `java/lang/ProcessHandle` interface delegate to the real `ProcessHandleImpl`
  instead of answering constants, `commandLine()` is registered, and the whole
  block is retagged `SyntheticStub`. See
  W7-10-processhandle-interface-stub-bodies.md.

What this record does NOT cover, and what W7-46 found on the same surface: the
`java/lang/Process` half of the registration loop. `Process.descendants()` is
concrete on the image, is NOT overridden by `java.lang.ProcessImpl`, and had no
`is_vm_process` guard — so under `--jdk-only` it read a pid slot off a JDK-layout
receiver and answered an empty stream. And `ProcessImpl.create` itself answered
a handle of `0` for a command line it could not parse. Both are in
W7-46-process-cluster.md.

**Nothing below has changed. Kept, not deleted, for the same reason its own
struck sections were: a retired record that is deleted gets rediscovered.**

---
**Status (reconciled 2026-08-12 — W7-55-record-reconciliation.md):**

* **Headline: CLOSED in source.** The ten `ProcessImpl` Windows natives and the
  corrected `create` descriptor are in `native-io/src/process.rs`. `signal_pid`
  is no longer POSIX-only: three `cfg` arms at `native-io/src/process.rs:1029`
  (unix), `:1061` (windows, `OpenProcess(PROCESS_TERMINATE)` + `TerminateProcess`),
  `:1097` (other).
* **Residual: MOSTLY CLOSED, but by a DIFFERENT MECHANISM — the
  `## Out-of-file patch (not applied)` below is superseded, not applied.** Do
  not apply it. It prescribes making `os_parent_pid`, `os_list_processes`,
  `collect_descendant_pids`, `process_scan_exception` and `ProcessScanError`
  `pub`, adding `p60_handle_stream`, and rewriting four bodies. **None of those
  identifiers exist anywhere in the tree** (grepped 2026-08-12). What actually
  landed is commit `0ab1067ec` *fix(jdk-only): route the ProcessHandle interface
  stubs at a real measurement*, which introduced `p60_real_handle_for`
  (`native-builtins/src/phases_late.rs:2416`) and `p60_delegate_to_real_handle`
  (`:2444`) — these invoke the real `ProcessHandleImpl` and pass its exceptions
  through untouched. On that mechanism: `children()` (`phases_late.rs:2644-2650`)
  and `descendants()` (`:2652-2664`) no longer fabricate empty streams;
  `parent()` (`p60_process_parent`, `:2522-2553`) no longer claims this VM as
  every process's parent, with a synthetic-only `Optional.empty()` fallback at
  `:2534`; and `ProcessHandle.current()`'s interception risk is addressed by
  retagging the block `SyntheticStub` (`:2621`, restated `:1639`) so `--jdk-only`
  drops it. Exactly **one line** of the recorded patch landed verbatim: the
  `commandLine` registration at `phases_late.rs:2826`, which closed a live
  `AbstractMethodError`.
* **Residual: CLOSED beyond what this record asked for.**
  `ProcessHandle$Info.command()`, which this record deliberately did *not*
  patch, is now pid-gated at `phases_late.rs:2760-2778`, and `info()`
  (`:2723-2738`) delegates to the real `ProcessHandleImpl$Info` first.
* **Residual: STILL OPEN — one.** Off POSIX, `ProcessHandle.destroy()` and
  `destroyForcibly()` still return `0` ("did not take effect"):
  `p60_handle_destroy` in `native-builtins/src/phases_late.rs` has
  `#[cfg(not(unix))] { let _ = force; Ok(Some(Value::Int(0))) }`. This record
  correctly identified `signal_pid` as the missing primitive and it now exists
  on Windows — but it is still a private `fn` in `native-io/src/process.rs`
  (`:1061`), not exported, so nothing consumes it. **This is the one live item
  in the record, and it is a two-line export plus a call.**
* **The 2026-08-11 amendment's point 1 is confirmed** — the *"Deliberately not
  fixed"* section is stale and superseded by W5-2; read it only as history.
* **Cannot adjudicate without a run:** whether `RJdkProcess` reaches
  `PASS RJdkProcess (53 checks)` under strict. Command:
  `target/release/cratonvm --jdk-only -cp regression-suite/build RJdkProcess`
  against `java -cp regression-suite/build RJdkProcess`. Note that a
  Compatible-mode measurement on 2026-08-12 found `RJdkProcess` failing on a
  **control** binary that pre-dates today's merges, with identical errors — so
  whatever it fails on is pre-existing, not a regression from recent work.

Filed 2026-08-07 by wave-3 lane W3-6. Source of truth for the census below is
`javap -p -s` against `C:\Program Files\Microsoft\jdk-25.0.3.9-hotspot`.

**AMENDED 2026-08-11** (jdk-only process-natives residual lane, also unbuilt).
Two changes, and they point in opposite directions:

1. **The "Deliberately not fixed" section below is STALE** — both bullets were
   superseded by W5-2 on 2026-08-07, four days before the records audit kept
   this record on the strength of them. Struck in place, with the amendment
   under each. Verified against `native-io/src/process.rs`, not inferred. This
   is the second direction of staleness the campaign has now found in this
   directory: not "a fix that never landed", but "a *refusal* that was
   reversed and never came back to the record that declared it".
2. **The "Found, not fixed — out of lane" section understated the problem.**
   It called the `java/lang/ProcessHandle` interface registrations inert
   outside synthetic-jdk mode. They are not: this lane's own
   `alloc_process_handle` mints an instance of that interface in real-JDK mode.
   Re-censused against `javap` (abstract / default / static), with a
   per-registration verdict and an out-of-file patch.

## The failure

```
RJdkProcess, --jdk-only (HotSpot 25 passes 53 checks, exit 0):
  java/lang/UnsatisfiedLinkError: java/lang/ProcessImpl.getStillActive()I
      at RJdkProcess.childProcess(RJdkProcess.java:177)
```

`:177` is `new ProcessBuilder(sleepLong())…start()`, the first `start()` in the
vector, and the class it initialises is the real Windows `ProcessImpl`:

```
static {};
  ...
  37: invokestatic  #491   // Method getStillActive:()I
  40: putstatic     #370   // Field STILL_ACTIVE:I
```

This is a **consequence of wave 2 succeeding**, not a regression. Strict mode
now drops the VM's `ProcessBuilder.start` shadow (stated `SyntheticStub`), the
image's own `start()` runs, and it constructs a real `java.lang.ProcessImpl` —
which is a class this VM had never actually executed before. Its entire native
surface was missing.

`--real-jdk` (compatible) mode fails the same vector at a **different** line and
for a different reason; see *The second failure* below.

## The census

Every `native` member of `ProcessImpl` / `ProcessHandleImpl` /
`ProcessHandleImpl$Info` on the JDK 25 Windows image, and its state before and
after this change. All are `ACC_NATIVE`, therefore all are §1.5 **`Bridge`**.

### `java.lang.ProcessImpl` (Windows image) — 10 natives

| method | descriptor | before | after |
| --- | --- | --- | --- |
| `create` | `(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;[JZ)J` | registered under `(Ljava/lang/String;[Ljava/lang/String;Ljava/lang/String;[JZ)J` — **could never bind** | descriptor corrected, body rewritten |
| `getStillActive` | `()I` | MISSING | added |
| `getExitCodeProcess` | `(J)I` | MISSING | added |
| `isProcessAlive` | `(J)Z` | MISSING | added |
| `terminateProcess` | `(J)V` | MISSING | added |
| `waitForInterruptibly` | `(J)V` | MISSING | added |
| `waitForTimeoutInterruptibly` | `(JJ)V` | MISSING | added |
| `getProcessId0` | `(J)I` | MISSING | added |
| `closeHandle` | `(J)Z` | MISSING | added |
| `openForAtomicAppend` | `(Ljava/lang/String;)J` | MISSING | added |

The wrong `create` descriptor is why the `--jdk-only` census (schema 3,
2026-08-05) recorded `ProcessImpl.create` in the *"do NOT resolve to an
`ACC_NATIVE` method … dead or shadowing"* bucket. It is not dead; it was
misspelled. `arg1` is a single **environment block** `String` (NUL-separated
`KEY=VALUE`, `null` to inherit), not a `String[]`.

### `java.lang.ProcessHandleImpl` — 7 natives, all already registered

`initNative()V`, `getCurrentPid0()J`, `isAlive0(J)J`,
`waitForProcessExit0(JZ)I`, `parent0(JJ)J`, `getProcessPids0(J[J[J[J)I`,
`destroy0(JJZ)Z`. No change.

`destroyProcess0(JZ)Z` is **also** registered and is **not** on the image —
superseded by `destroy0(JJZ)Z` since JDK 9. Left alone; it is inert, and it is
already recorded in the retired L5 `native-io` bridge-residuals write-up
(`retired/l5-native-io-bridge-residuals-RETIRED-20260810.md`, internal tree).
Its class *is* on the image, so it is a `method-nowhere` row and stays on the
deletion-candidate list; only the `class-absent` bucket was re-tagged on
2026-08-10.

### `java.lang.ProcessHandleImpl$Info` — 2 natives, both already registered

`initIDs()V`, `info0(J)V`. No change to the registration; see *Deliberately not
fixed* for `info0`'s Windows behaviour.

## What the handle is

`create` returns this VM's **process-table key**, which the JDK stores in the
private `ProcessImpl.handle` field and hands back to every other native above.
It is not an OS handle, so none of the new natives makes a Win32 call — the
table already knows the child. This is the same substitution the Linux arm makes
with the pid.

The one native that must translate is `getProcessId0`: the constructor does
`processHandle = ProcessHandleImpl.getInternal(getProcessId0(handle))`, so it
answers the real OS pid via `pid_for_handle`, and everything downstream
(`pid()`, `toHandle()`, `parent()`, `children()`, and the reaper's
`completion(pid, true)`) hangs off that.

## Where a fabricated success was avoided

* **`getExitCodeProcess` / `isProcessAlive` on a handle this VM does not own.**
  `try_exit_handle` answers `None` both for "still running" and for "no such
  child". Reading the second as the first makes `isAlive()` claim a nonexistent
  process is running and `waitFor()` never return. A `known_handle` guard
  (`exit_cache` membership, which outlives the child) separates them; an unknown
  handle answers `-1` / `false`.
* **The 259 ambiguity is not papered over.** `getExitCodeProcess` returns
  `STILL_ACTIVE` (259) for a running child, so a child that genuinely exits with
  259 is momentarily indistinguishable. HotSpot has the identical ambiguity and
  resolves it in bytecode — `exitValue()` re-checks `isProcessAlive(handle)`
  before throwing and asks again — which our pair answers correctly. No
  invention needed.
* **`closeHandle` returns `true` without closing anything.** Justified, not
  fabricated: the value is a table key, the OS handle it stands for is owned by
  the `std::process::Child` whose `Drop` closes it, and dropping the table row
  here would break the pid-keyed `ProcessHandleImpl` natives that outlive the
  `ProcessImpl` object by design.

## The second failure — `--real-jdk`, `RJdkProcess.java:185`

Same vector, different arm, measured on the same build:

```
java/lang/AssertionError: a live forked child must report a parent
    at RJdkProcess.childProcess(RJdkProcess.java:185)
```

`lh.parent()` on a real `ProcessHandleImpl` runs real bytecode into
`parent0(pid, startTime)`, and `os_parent_pid` / `os_list_processes` /
`direct_child_pids` were **Linux-only**, answering `-1` and the empty list
everywhere else. That is the wave-2 bug species again: "this process has no
parent" and "the machine is running no processes" are indistinguishable from
true answers, so `ProcessHandle.parent()` was permanently empty and
`children()` / `descendants()` / `allProcesses()` permanently empty streams
(`:185`, `:188`, `:189`).

Fixed with a single `CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS)` reader
(`os_snapshot_processes`) that all three derive from, so they cannot disagree
about the shape of the tree. Same call HotSpot's `ProcessHandleImpl_md.c` makes.

**Incidentally required, and load-bearing for the strict arm:** `signal_pid` was
a flat `false` off POSIX, on the stated grounds that the case needing it was
Linux-only. It stopped being Linux-only the moment the real `ProcessImpl` ran:
its constructor parks a reaper thread inside `Child::wait()` for every child, so
`destroy_handle`'s `try_lock` always fails on Windows and the pid route is the
only one left — `destroy()` / `destroyForcibly()` terminated nothing.
`RJdkProcess.java:206` measures it. Now `OpenProcess(PROCESS_TERMINATE)` +
`TerminateProcess(h, 1)`, exactly HotSpot's own call and exit code.

## Deliberately not fixed — **BOTH BULLETS ARE STALE, SUPERSEDED 2026-08-07**

> **Do not act on this section.** It was written by wave 3 and overtaken the
> same day by wave 5 (W5-2), which fixed both items for a reason wave 3 could
> not see: the `STARTTIME_ANY` 0 that bullet one calls "consistent" is what
> `ProcessHandleImpl$Info.info(pid, startTime)` compares, and that test is a
> **bare `!=` with no wildcard**, so it WIPED the whole record. Bullet one's own
> consistency argument enumerates `equals`, `children()` and `descendants()` —
> all three of which do wildcard 0 — and misses the one caller that does not.
>
> Verified against `native-io/src/process.rs` on 2026-08-11, not inferred:
> `os_process_start_time` has a `#[cfg(windows)]` arm delegating to
> `win_process_times` (`GetProcessTimes`); `os_process_image_name`
> (`QueryFullProcessImageNameW`) exists; `native_proc_handle_info0` writes
> `startTime` unconditionally and fills `command` from the image name on
> Windows. **The 2026-08-11 records audit kept W3-6 on the stated grounds
> "Windows `info0`/`start_time` empty" — that reason was these two bullets, and
> it had been false for four days.** Kept, struck, rather than deleted: a
> retired claim that is deleted gets rediscovered.
>
> **Amendments below each bullet, 2026-08-11.**

* **`os_process_start_time` stays `None` on Windows.** Every start time is
  therefore `STARTTIME_ANY` (0). Verified against the image's bytecode that this
  is consistent rather than merely convenient: `ProcessHandleImpl.equals` accepts
  when either side is 0, `children()`'s filter is `this.startTime <=
  child.startTime` (`0 <= 0`), and `descendants()` seeds its threshold from the
  same array it filters. Reporting a real `GetProcessTimes` value would also
  work but would have to be made consistent across `isAlive0`,
  `getProcessPids0` and `destroy0` at once, and nothing measured needs it.

  > **SUPERSEDED.** It IS reported, and it was made consistent across all three
  > at once — through the single `start_time_or_any(pid)`, which is what
  > `isAlive0`, `info0` and `current_process_start_time` now all call. "Nothing
  > measured needs it" was wrong by two `RJdkProcess` checks.

* **`ProcessHandleImpl$Info.info0` stays empty on Windows.** `os_process_cmdline`
  has no cheap Win32 equivalent — the command line lives in the target's PEB.
  `QueryFullProcessImageNameW` would give the image path, but `info0` fills
  `command`, `commandLine` and `arguments` together, and synthesising a
  `commandLine` from an image path with no arguments is precisely the fabricated
  success this wave is hunting. Empty `Optional`s are what the JDK specifies for
  a platform that cannot report, and `RJdkProcess.java:127-137` asserts only the
  shape.

  > **HALF SUPERSEDED — and the surviving half is the interesting half.**
  > `info0` is no longer empty on Windows: it fills `command` (the image path,
  > from `QueryFullProcessImageNameW`), `startTime` and `totalTime` (both from
  > one `GetProcessTimes`), and since 2026-08-11 `user` (token SID ->
  > `LookupAccountSidW`).
  >
  > What this bullet got RIGHT, and what still stands, is the refusal to
  > synthesise `commandLine` and `arguments` from an image path. Real HotSpot 25
  > on this host measures `commandLine = Optional.empty` and `arguments =
  > Optional.empty` while `command = Optional[…\java.exe]`, so filling the first
  > two would diverge from the oracle in the *other* direction.
  >
  > The premise that had to go was "`info0` fills `command`, `commandLine` and
  > `arguments` together". It does not have to; they are three independent field
  > writes, and treating them as one unit is what made a reportable value look
  > unreportable. **That is the generalisable error in this bullet** — not the
  > conclusion about `commandLine`, which was correct, but the coupling that
  > made the conclusion swallow `command` with it.

## Found, not fixed — out of lane. **Re-examined 2026-08-11 against `javap`; the verdict changes.**

`native-builtins/src/phases_late.rs` registers `children()` and `descendants()`
on the `java/lang/ProcessHandle` **interface** returning **empty streams**, and
`parent()` returning `p60_parent_pid()` — which off POSIX is
`std::process::id()`, i.e. "every process's parent is this VM". Both are
fabricated successes of the named species.

They are inert in `--real-jdk` and `--jdk-only`, where the receiver is a real
`ProcessHandleImpl` and its own bytecode wins — that is exactly what the `:185`
failure above *proves*, since `p60_process_parent` would have returned a
**present** `Optional`. They are the only implementation in **synthetic-jdk**
mode, which this lane cannot build or run. Left for a lane that can measure the
synthetic-jdk gate.

### The census wave 3 did not take: what is abstract, default and static

`javap -p -s java.lang.ProcessHandle` and `… 'java.lang.ProcessHandle$Info'`,
JDK 25.0.3 (`C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot`). This
split is the whole argument, and neither this record nor W5-2 had it:

| surface | kind on the image |
| --- | --- |
| `ProcessHandle.pid parent children descendants info onExit supportsNormalTermination destroy destroyForcibly isAlive hashCode equals compareTo(ProcessHandle)` | **abstract** (13) |
| `ProcessHandle.of(J) current() allProcesses()` | **static** (3) |
| `ProcessHandle.compareTo(Object)` | **default** (1, the `Comparable` bridge) |
| `ProcessHandle$Info.command commandLine arguments startInstant totalCpuDuration user` | **abstract** (6, all of them) |

**No method on either interface is `ACC_NATIVE`.** So by contract §1.5 not one
of these registrations is a `Bridge`, and the ambient `NativeKind::Bridge` that
`register_p60_process_handle` sets over the whole block is misstated for every
row in it — the same misstatement §5 of the review already forced on
`cratonvm/synthetic/Process*` and on `ProcessBuilder.start`.

### Why they are nevertheless load-bearing, and must not simply be deleted

This is the correction to "left for a lane that can measure the synthetic-jdk
gate". They are **not** synthetic-jdk-only in effect, because a receiver whose
runtime class *is* `java.lang.ProcessHandle` gets minted in real-JDK mode too,
by this very lane's own code:

* `native-io/src/process.rs`, `alloc_process_handle` — `ensure_class_initialized
  ("java/lang/ProcessHandle")` then `alloc_object`, i.e. an instance of the
  **interface**. It is the fallback arm of `build_process_handle`, reached
  whenever `new_object_initialized("java/lang/ProcessHandleImpl", "(JJ)V", …)`
  fails.
* `native-builtins/src/phases_late.rs`, `p60_process_handle_current` and
  `p60_process_parent` — `try_alloc_concurrent_synthetic(ctx,
  "java/lang/ProcessHandle", 1)`.

On such a receiver every call resolves to the interface declaration, and for an
**abstract** declaration there is no `Code` to run: an unregistered triple is a
hard `AbstractMethodError: … has no Code attribute`, with no bytecode fallback,
because the object was never a `ProcessHandleImpl`. That is the W7-5 mechanism
(docs/known-issues/jdk-only/W7-5-registrars-that-never-shipped.md §3), and
`native_process_to_handle`'s own doc comment records it being hit empirically:
*"`ensure_class_initialized` succeeds for `java/lang/ProcessHandle` even without
`--java-home`, so the `Err(_)` synthetic-fallback branch this VM uses elsewhere
never triggers here"*.

So the 13 abstract `ProcessHandle` rows and the 6 abstract `Info` rows are the
**only** thing standing between that fallback receiver and an
`AbstractMethodError`. **The defect is in their bodies, not in their placement.**

### The other direction, and why it does not bite here

The symmetric hazard — a native registered on a real interface intercepting
every instance, *including* a genuine `ProcessHandleImpl` — is real in general
and is why this section exists. It is measured NOT to be happening on these
triples, and the measurement is W5-2's, from the check counter rather than from
reasoning: if the interface `info()` stub had intercepted a real
`ProcessHandleImpl` receiver, `RJdkProcess` would have counted **52**, not 51
(`command()` present, `startInstant()` skipped), and `commandLine()` — which has
no stub on that class — would have raised `AbstractMethodError` instead of
letting the run finish. Neither happened. Independently: `RJdkProcess:186`
passes, which it could not if the `parent()` stub were intercepting.

That is evidence about **this dispatch path with this receiver**, not a general
licence. A real class that declares its own bytecode wins; the interception risk
lives on triples where the real class does *not* override.

### Per-registration verdict

`register_p60_process_handle` (`native-builtins/src/phases_late.rs`), and the
three-row duplicate of it in `register_phase57_process` (`current`, `pid`,
`isAlive` — same function pointers, so no divergence).

| registration | kind on image | verdict |
| --- | --- | --- |
| `ProcessHandle.current()` | **static** | **Dangerous, highest priority.** Static interface methods keep the native check in real-JDK mode (W7-5 §4.2, quoting `phases_late/streams.rs`), so unlike its abstract neighbours this one really can intercept — and it returns a 1-field synthetic where the JDK's own `current()` returns the `ProcessHandleImpl` singleton. Needs the treatment `Gatherer.defaultInitializer` got: keep, but only where no real image class exists. |
| `ProcessHandle.pid()` | abstract | **Keep.** Reads field 0, which is the layout `alloc_process_handle` writes. Correct for a minted receiver, unreachable on a real one. |
| `ProcessHandle.isAlive()` | abstract | **Keep.** |
| `ProcessHandle.compareTo(ProcessHandle)` | abstract | **Keep.** Compares pids; the constant-`0` version that collapsed a `TreeSet` is already fixed in place. |
| `ProcessHandle.supportsNormalTermination()` | abstract | **Keep.** `cfg!(unix)` is the real platform answer, not a placeholder. |
| `ProcessHandle.destroy()` / `destroyForcibly()` | abstract | **Keep** on POSIX. Off POSIX the body returns `0` ("did not take effect") — honest, and `supportsNormalTermination` agrees. See the Windows out-of-file patch below: `native-io`'s `signal_pid` is exactly the missing primitive, and it is already `pub`-adjacent in this crate. |
| `ProcessHandle.children()` | abstract | **FABRICATED — fix in place.** A hardcoded empty stream is "this process has no children", indistinguishable from the true answer. `native-io`'s `os_list_processes` / `collect_descendant_pids` already answer this correctly on both platforms **and now raise `RuntimeException` on a failed scan** rather than answering empty. |
| `ProcessHandle.descendants()` | abstract | **FABRICATED — fix in place.** Same, via `collect_descendant_pids`. |
| `ProcessHandle.parent()` | abstract | **FABRICATED, worst of the set.** `p60_parent_pid()` off POSIX is `std::process::id()` — "every process's parent is this VM" — and it ignores its receiver entirely, so it answers the same thing for a handle to any process. `native-io`'s `os_parent_pid` is the correct probe and is now fallible. |
| `ProcessHandle.onExit()` | abstract | **Suspect.** Returns an already-completed future for a process that may still be running. Not re-examined by this lane. |
| `ProcessHandle.info()` | abstract | **FABRICATED.** Returns a 0-field synthetic `ProcessHandle$Info` whose accessors are the stubs below, bypassing `ProcessHandleImpl$Info.info(pid, startTime)` and therefore `info0` and everything this lane fixed in it. |
| `ProcessHandle$Info.command()` | abstract | **FABRICATED.** Answers `Optional[std::env::current_exe()]` for **every** receiver regardless of pid — this VM's executable presented as the target process's. The comment defending it names a real caller (Spring's `ClassPathManifestEntries`) that wants `current()`'s command, which is the one receiver for which the answer is accidentally right. |
| `ProcessHandle$Info.arguments()` / `user()` / `startInstant()` / … | abstract | **Correct as far as they go.** `Optional.empty()` is the specified answer for a value the implementation does not have (javadoc: *"The attributes of a process vary by operating system and are not available in all implementations"*), so these are honest absences, not fabrications. They should be *upgraded* to real values, not merely defended. |
| `ProcessHandle$Info.commandLine()` | **absent** | **A live `AbstractMethodError`.** Abstract on the interface, registered nowhere, and the receiver `info()` mints is an instance of the interface. W5-2 relied on this: it is why the interface stub is known not to be intercepting. Any fix to `info()` must add it. |

## Out-of-file patch — SUPERSEDED, DO NOT APPLY

> **Reconciled 2026-08-12.** This patch was never applied and must not be. Its
> *diagnosis* was right and was acted on; its *prescription* was overtaken. None
> of the identifiers it introduces — `pub fn os_parent_pid`,
> `pub fn os_list_processes`, `pub fn collect_descendant_pids`,
> `pub fn process_scan_exception`, `pub struct ProcessScanError`,
> `p60_handle_stream` — exists anywhere in the tree. The four fabricated bodies
> it targets were instead fixed by commit `0ab1067ec` via delegation to the real
> `ProcessHandleImpl` (`p60_real_handle_for`,
> `native-builtins/src/phases_late.rs:2416`; `p60_delegate_to_real_handle`,
> `:2444`), which is the better answer because it passes the real
> implementation's exceptions through untouched. The one line of this patch that
> DID land verbatim is the `commandLine` registration, now at
> `phases_late.rs:2826`. The one thing it prescribes that is still LIVE is the
> Windows `destroy()`/`destroyForcibly()` route through `signal_pid` — see the
> status block at the top of this record. Kept below as the record of the census.

Owned by another lane this wave; recorded here with the exact code rather than
described. The four fabricated bodies in
`native-builtins/src/phases_late.rs`'s `register_p60_process_handle` should
delegate to the probes `native-io` already exports, which are the same ones the
real `ProcessHandleImpl` natives use — so the two implementations cannot report
different process trees.

`native-io/src/process.rs` would need these four `pub`, all currently private
(this lane did not make them `pub`, because an export with no consumer is dead
code the next census flags):

```rust
pub fn os_parent_pid(pid: i64) -> Result<i64, ProcessScanError>;
pub fn os_list_processes(of_pid: i64) -> Result<Vec<(i64, i64)>, ProcessScanError>;
pub fn collect_descendant_pids(pid: i64) -> Result<Vec<i64>, ProcessScanError>;
pub fn process_scan_exception(ctx: &mut dyn NativeContext, err: ProcessScanError) -> MethodCallFailed;
```

`ProcessScanError` would have to become `pub` with it. Then, in
`register_p60_process_handle`:

```rust
    // A hardcoded empty stream is the fabricated success this campaign is
    // named for: "this process has no children" and "this VM cannot enumerate
    // processes" are different facts and only one of them is true. Same probe
    // `ProcessHandleImpl.getProcessPids0` uses, so the two cannot disagree
    // about the shape of the tree — and it THROWS on a failed scan, which is
    // what HotSpot's `ProcessHandleImpl_md.c` does.
    r.register(ph, "children", "()Ljava/util/stream/Stream;", |ctx, args| {
        let pid = p60_handle_pid(ctx, args).unwrap_or(0);
        let pid = if pid <= 0 { std::process::id() as i64 } else { pid };
        let kids = match cratonvm_native_io::process::os_list_processes(pid) {
            Ok(found) => found.into_iter().map(|(c, _)| c).collect::<Vec<_>>(),
            Err(e) => return Err(cratonvm_native_io::process::process_scan_exception(ctx, e)),
        };
        p60_handle_stream(ctx, &kids)
    });

    r.register(ph, "descendants", "()Ljava/util/stream/Stream;", |ctx, args| {
        let pid = p60_handle_pid(ctx, args).unwrap_or(0);
        let pid = if pid <= 0 { std::process::id() as i64 } else { pid };
        let kids = match cratonvm_native_io::process::collect_descendant_pids(pid) {
            Ok(found) => found,
            Err(e) => return Err(cratonvm_native_io::process::process_scan_exception(ctx, e)),
        };
        p60_handle_stream(ctx, &kids)
    });

    // `p60_parent_pid()` ignored the receiver and answered `std::process::id()`
    // off POSIX — "every process's parent is this VM", for a handle to any
    // process on the machine. `-1` is the JDK's own "no parent" and is the ONLY
    // honest empty; a scan that could not run throws.
    r.register(ph, "parent", "()Ljava/util/Optional;", |ctx, args| {
        let pid = p60_handle_pid(ctx, args).unwrap_or(0);
        let pid = if pid <= 0 { std::process::id() as i64 } else { pid };
        let ppid = match cratonvm_native_io::process::os_parent_pid(pid) {
            Ok(p) => p,
            Err(e) => return Err(cratonvm_native_io::process::process_scan_exception(ctx, e)),
        };
        if ppid <= 0 {
            return p60_empty_optional(ctx, &[]);
        }
        let parent = try_alloc_concurrent_synthetic(ctx, "java/lang/ProcessHandle", 1)?;
        ctx.set_field(parent, 0, Value::Long(ppid));
        let parent_pin = ctx.pin_native_root(parent);
        let optional = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
        let parent = ctx.read_native_pin(parent_pin, parent);
        ctx.unpin_native_roots(parent_pin);
        ctx.set_field(optional, 0, Value::Object(Some(parent)));
        Ok(Some(Value::Object(Some(optional))))
    });

    // ABSTRACT on `java.lang.ProcessHandle$Info` (`javap -p -s`, JDK 25.0.3) and
    // registered NOWHERE, while the receiver `info()` mints is an instance of
    // that interface — so this triple is an `AbstractMethodError: has no Code
    // attribute` waiting for its first caller. It is only unhit because the
    // `info()` stub is currently unreachable from a real `ProcessHandleImpl`
    // (which is itself W5-2's evidence that the stub does not intercept).
    r.register(phi, "commandLine", "()Ljava/util/Optional;", p60_empty_optional);
```

`p60_handle_stream(ctx, &[i64]) -> MethodCallResult` is a small helper that
does not exist yet: allocate a `ProcessHandle` per pid with field 0 set, put
them in a reference array, wrap in the `STREAM_NUM_FIELDS` layout
`native-collections`' `make_stream` uses (**not** the 1-field one — W7-5 §4.2
reason 2), pinning across each allocation.

**`ProcessHandle$Info.command()` is deliberately NOT patched above**, because
the honest fix is not a one-liner: it must take the receiver's pid and call the
same probe `info0` uses, and the current stub has no pid — the `Info` object
`info()` mints has zero fields. Fixing it means giving that synthetic `Info` a
pid slot, or making `info()` build a real `ProcessHandleImpl$Info` through
`Info.info(pid, startTime)` and letting `info0` do the work, which is the better
shape and subsumes the whole `$Info` stub block. That is a design decision, not
a patch, and it belongs to whichever lane owns `phases_late`.

## The single falsifying observation

Strict-mode `RJdkProcess` reaching `PASS RJdkProcess (53 checks)`. Anything
short of that — in particular a *different* `UnsatisfiedLinkError` naming a
`ProcessImpl` or `ProcessHandleImpl` member — falsifies the claim that the
census above is complete.
