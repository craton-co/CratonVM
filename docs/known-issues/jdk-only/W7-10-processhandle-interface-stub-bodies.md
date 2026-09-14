# W7-10 — `ProcessHandle` / `ProcessHandle$Info`: the stub bodies, not the stub placement

**Status:** fixed in `native-builtins/src/phases_late.rs`, 2026-08-11. Four
fabricated bodies replaced with the measurement `native-io/src/process.rs`
already takes, one unregistered abstract method registered, and the ambient
`NativeKind::Bridge` over the whole block restated as `SyntheticStub` at both
registration sites. Three residuals are named in §7 and two of them are
out-of-file.

> **§7.3 APPLIED 2026-08-12, unbuilt.** The one-line row is in
> `classloading/src/class_manager.rs`, so §4's `commandLine` registration is no
> longer real-JDK-only. §7.1 (`onExit` on a minted handle) and §7.2 (the
> synthetic-JDK empty stream) are untouched and remain the record's open work.

> **RE-VERIFIED AGAINST THE TREE 2026-08-12 (later pass, A9 record triage).**
> Read only — nothing was built or run in that pass either. Line numbers are
> against the **committed** tree at `768ac2de0`; `classloading/src/class_manager.rs`
> carries a concurrent lane's uncommitted edits, but §7.3's two rows are in the
> commit and at the lines cited.
>
> * **The fix is present and is what §3–§5 describe.** `register_phase57_process`
>   opens its three shared triples with an explicit
>   `r.set_category(NativeKind::SyntheticStub)` and restores the ambient
>   category after (`native-builtins/src/phases_late.rs`, the
>   `let __ph_cat = r.current_category();` block), and
>   `register_p60_process_handle` carries §5's whole argument in its doc comment
>   and its own `set_category`. Both sites, as §5 requires.
> * **§4's row is live.**
>   `r.register(phi, "commandLine", "()Ljava/util/Optional;", p60_empty_optional)`
>   is the last registration in `register_p60_process_handle`, and §7.3's
>   `mk("commandLine", "()Ljava/util/Optional;")` is at
>   `classloading/src/class_manager.rs:15464` with the mirror exact-set assertion
>   at `:19294`.
> * **§2's "no other file registers either class name" reproduces.** The only
>   other `ProcessHandle` mentions in Rust are comments
>   (`native-io/src/lib.rs`, `native-builtins/src/lib.rs`, `vm/src/vm/vm_init.rs`,
>   `native-builtins/src/phases_late/reflect_invoke.rs` — the last is a stale
>   section header above `p60_empty_optional`, not a registrar), the
>   `ProcessHandleImpl` natives in `native-io/src/process.rs` (a *different* class
>   name), the carrier in `class_manager.rs`, and the two tests. No competing
>   registration exists, so last-write-wins has nothing to decide here.
> * **§6's "second finding" HAS BEEN FIXED by another lane, and §6's numbers are
>   dead.** `native-builtins/tests/common/vm_init_boot_path.rs` now names
>   `register_p60_process_handle` and `register_classvalue_natives` in
>   `VM_INIT_SEQUENCE` (`:114`/`:115`) and calls them in the replay
>   (`:233`/`:234`), and it grew a **source witness** —
>   `vm_init_real_jdk_boot_path` reads `vm_init.rs`'s real-JDK arm and fails on
>   an unmodelled registrar or an order inversion. Its failure message cites this
>   exact defect: *"18 registrations, `register_p60_process_handle` and
>   `register_classvalue_natives`"*. `BASELINE_SYNTHETIC_STUBS` is no longer
>   1038; it is now a pair, `BASELINE_SYNTHETIC_STUBS_MANAGEMENT = 1263` /
>   `_NO_MANAGEMENT = 1253`, selected by `cfg`. **Do not quote §6's
>   1038 -> 1041, nor its bridge-ratchet deltas**: they were computed against a
>   census scope that has since changed and a baseline that has since been
>   re-frozen. §6's *reasoning* (a stub census cannot tell a new fake from a
>   correctly re-tagged old one) is what survives. That fix has its own record:
>   `docs/known-issues/jdk-only/W7-30-stub-ratchet-boot-path-scope.md`, which is
>   where a reader should go for the current scope and baselines rather than to
>   §6.
> * **Which census category this record is, since the two are counted
>   separately.** Seventeen of the eighteen instance rows are *natives
>   intercepting an **abstract** declaration on a real image class* — the 2865
>   category, not the 335 "fabricated method on a real class" one. No method
>   registered here is absent from the image: `javap` declares all thirteen
>   `ProcessHandle` abstracts and all six `$Info` abstracts, `commandLine`
>   included. The one row in a different category is `current()`, which is
>   `ACC_STATIC` **with `Code`** — a §1.4 shadow of real bytecode. "No `Code`
>   attribute" on the other rows means *abstract*, which is exactly why the
>   registrations are load-bearing against `AbstractMethodError`, and does not
>   mean a broken dispatch.
> * **§7.1 and §7.2 are still open, unchanged.** `onExit` still answers a
>   `p58_new_cf(ctx, Value::Object(None), true)` — an already-completed future —
>   with no `p60_delegate_to_real_handle` attempt, so unlike `info`/`parent`/
>   `children`/`descendants` it does not delegate even on a real image.

**Nothing here has been built or run.** Every claim is either `javap` output
from the JDK 25 image on this host (Eclipse Adoptium jdk-25.0.3.9-hotspot,
`javap -version` = `25.0.3`), the JDK's own `lib/src.zip`, or a read of the
tree. No measurement is claimed and no test result is reported.

---

## 1. What the previous lane got right, and the one thing it got wrong

`W3-6-processimpl-missing-natives.md` filed these registrations as *"inert
outside synthetic-jdk"*. The lane that finished `native-io/src/process.rs`
refuted that: `alloc_process_handle` mints an object whose runtime class is the
`java/lang/ProcessHandle` **interface**, and `try_alloc_concurrent_synthetic`
does the same here, so the abstract registrations are load-bearing against
`AbstractMethodError` and the defect is in their **bodies**. That is correct and
is what this record acts on.

One correction to that refutation, verified here. In **real-JDK** mode the mint
is *fallback-gated*, not routine. `alloc_process_handle` has exactly one caller
(`native-io/src/process.rs`, `build_process_handle`'s `_ =>` arm) and it runs
only when `new_object_initialized("java/lang/ProcessHandleImpl", "(JJ)V", …)`
has already failed; `p60_process_handle_current`'s mint is the same arm of the
same construction. So on a complete JDK 25 image the interface-classed receiver
appears only when `ProcessHandleImpl` cannot be constructed — and, before this
change, from **`parent()`, unconditionally**, which minted one on every call.
That was the largest real-JDK source of these receivers and it is now gone.

The practical shape is therefore: these bodies serve synthetic-JDK mode
routinely, real-JDK mode only on a degraded image, and `--jdk-only` never (§5).

## 2. The surface, re-derived

`javap java.lang.ProcessHandle` and `javap 'java.lang.ProcessHandle$Info'`,
JDK 25.0.3. "Registered" = registered by `register_p60_process_handle` and/or
`register_phase57_process`, both of which run in the default
(`synthetic-jdk`-off) build — the first at `vm/src/vm/vm_init.rs:2406`, the
second via `register_essential_natives_with_shims`. No other file in the tree
registers anything on either class name (checked by grep across all crates: the
only other mentions of `"java/lang/ProcessHandle"` are
`ensure_class_initialized` / `refused_class` in `native-io/src/process.rs`).

### `java.lang.ProcessHandle` — 13 abstract, 3 static, 1 default

| method | descriptor | kind on the image | registered | body after this change |
|---|---|---|---|---|
| `pid` | `()J` | abstract | yes (×2) | slot 0 — the mint's only fact |
| `parent` | `()Ljava/util/Optional;` | abstract | yes | **delegates** (was: this VM's `getppid()`) |
| `children` | `()Ljava/util/stream/Stream;` | abstract | yes | **delegates** (was: empty stream) |
| `descendants` | `()Ljava/util/stream/Stream;` | abstract | yes | **delegates** (was: empty stream) |
| `info` | `()Ljava/lang/ProcessHandle$Info;` | abstract | yes | **delegates** (was: 0-field mint) |
| `onExit` | `()Ljava/util/concurrent/CompletableFuture;` | abstract | yes | unchanged — see §7.1 |
| `supportsNormalTermination` | `()Z` | abstract | yes | unchanged (real: `cfg!(unix)`) |
| `destroy` | `()Z` | abstract | yes | unchanged (real `kill`) |
| `destroyForcibly` | `()Z` | abstract | yes | unchanged (real `kill`) |
| `isAlive` | `()Z` | abstract | yes (×2) | unchanged (real `kill(pid,0)`) |
| `hashCode` | `()I` | abstract | **no** | — see §4 |
| `equals` | `(Ljava/lang/Object;)Z` | abstract | **no** | — see §4 |
| `compareTo` | `(Ljava/lang/ProcessHandle;)I` | abstract | yes | unchanged (compares pids) |
| `of` | `(J)Ljava/util/Optional;` | **static** | **no** | real bytecode |
| `current` | `()Ljava/lang/ProcessHandle;` | **static** | yes (×2) | unchanged body, re-tagged — §5 |
| `allProcesses` | `()Ljava/util/stream/Stream;` | **static** | **no** | real bytecode |
| `compareTo` | `(Ljava/lang/Object;)I` | **default** | **no** | real bytecode — it has `Code` |

Coverage: **11 of 13 abstracts** registered, 1 of 3 statics, 0 of 1 default.

### `java.lang.ProcessHandle$Info` — 6 abstract, 0 static, 0 default

| method | descriptor | registered before | registered after |
|---|---|---|---|
| `command` | `()Ljava/util/Optional;` | yes — `current_exe()` for **any** pid | yes — `current_exe()` for **this** pid only |
| `commandLine` | `()Ljava/util/Optional;` | **no** | **yes** — §4 |
| `arguments` | `()Ljava/util/Optional;` | yes — empty | yes — empty |
| `startInstant` | `()Ljava/util/Optional;` | yes — empty | yes — empty |
| `totalCpuDuration` | `()Ljava/util/Optional;` | yes — empty | yes — empty |
| `user` | `()Ljava/util/Optional;` | yes — empty | yes — empty |

Coverage after: **6 of 6**.

## 3. The fix: delegate, do not reimplement

Every instance registration above is reachable only from a receiver whose
runtime class *is* the interface. All three native-shadow hierarchy walks in the
tree are `superclass` walks and none walks interfaces —
`vm/src/runtime/interpreter/invoke.rs`'s step-1 `or_else`
(`let parent_id = cm.get_class(cid)?.superclass?;`) and `dispatch_virtual.rs`'s
vtable fast path and `populate_virtual_invoke_cache`. An interface cannot be
instantiated, so that receiver is always a mint, and a mint carries one fact:
the pid in slot 0. `children`, `descendants`, `parent` and `info` are OS
measurements; slot 0 is not one. That gap is why they answered constants.

The measurements exist and are already correct, in `native-io/src/process.rs`'s
`getProcessPids0`, `parent0` and `Info.info0` natives, which the JDK's own
`ProcessHandleImpl` bytecode calls. They are **private** functions of that file
(`os_list_processes`, `os_parent_pid`, `collect_descendant_pids`,
`process_scan_exception`, `ProcessScanError`) and that file belongs to another
lane, so the patch W3-6 recorded — which asks for four `pub`s — could not be
applied. It did not need to be: the supported route reaches the same probes
through the JDK, without a second copy of the process table to drift out of
step with the first.

```rust
fn p60_real_handle_for(ctx: &mut dyn NativeContext, args: &[Value]) -> Option<ObjectRef> {
    let pid = p60_handle_pid(ctx, args)?;
    match ctx.invoke(
        "java/lang/ProcessHandleImpl",
        "getInternal",
        "(J)Ljava/lang/ProcessHandleImpl;",
        &[Value::Long(pid)],
    ) { Ok(Some(Value::Object(Some(real)))) => Some(real), _ => None }
}
```

Three things decide this shape:

* **`getInternal`, not the `(JJ)V` constructor.** `javap -p
  java.lang.ProcessHandleImpl` declares `static ProcessHandleImpl
  getInternal(long)`, whose body is `new ProcessHandleImpl(pid, isAlive0(pid))`.
  The second argument is the entire point. A hardcoded `0` is `STARTTIME_ANY`,
  which `ProcessHandleImpl$Info.info(long, long)` does **not** honour: its check
  is a bare `startTime != info.startTime`, and on a mismatch it nulls
  `command`, `arguments`, `startTime`, `totalTime` and `user` on the record
  `info0` has just filled in. That is W5-2's defect. Sourcing the start time
  from `isAlive0` — the same function `info0`'s counterpart answers with —
  makes the two agree by construction, so no later edit can put them out of
  step. docs/known-issues/jdk-only/W5-2-two-silently-skipped-process-checks.md.
* **`ctx.invoke` reaches a static.** `W7-9` §8.2 declined `Selector.provider()`
  on the grounds that `NativeContext` has *"no `invoke_static`"*. There is no
  method of that name, but `NativeInvokeAccess::invoke` is name-based and
  already serves statics all over the tree — `Optional.of`, `Optional.empty`
  and `Integer.valueOf` in `native-builtins/src/lang_system.rs`,
  `Optional.ofNullable` in `net_phase_e.rs`. **W7-9 §8.2's stated blocker does
  not exist**; that patch is applicable as written with `invoke` substituted.
  Recorded here rather than edited into W7-9, which is another lane's file.
* **The delegated result is returned untouched, exceptions included.**
  `ProcessHandleImpl.children(long)` funnels into `getProcessPids0`, which
  `native-io/src/process.rs` raises a `java.lang.RuntimeException` from when the
  enumeration syscall fails — the same thing HotSpot's `ProcessHandleImpl_md.c`
  and `ProcessHandleImpl_unix.c` do. Catching it and answering an empty stream
  would reinstate the fabricated success one layer down.

**No recursion.** The delegated receiver's class is `ProcessHandleImpl`, which
declares its own `children`/`descendants`/`parent`/`info` bytecode, so
`invoke.rs`'s `has_own_bytecode` early return fires and the interface
registration is never consulted for it.

### What each fabrication was, and what replaced it

* **`children()` / `descendants()`** — a hardcoded empty stream. `children()`'s
  javadoc carries **no absence clause**: *"@return a sequential Stream of
  ProcessHandles for processes that are direct children of the process"*. An
  empty stream therefore asserts "this process has no children", which is a
  measurement, and it was indistinguishable from "this VM cannot enumerate
  processes". Now delegated. The old body also allocated a **one-field**
  `java/util/stream/Stream` while `STREAM_NUM_FIELDS` in `native-collections` is
  **2** (slot 1 is the `BaseStream.onClose` handler array); the fallback now
  goes through the `pub` `make_stream_from_elements`, which is the constructor
  that file documents for exactly this use.
* **`parent()`** — ignored the receiver entirely and answered `getppid()`, i.e.
  "every process's parent is this VM's parent", for a handle to any process on
  the machine; and wrapped it in a fresh bare-interface mint that could not
  answer anything about the process it named. Now delegated to `parent0(pid,
  startTime)`, which also means the `Optional` carries a *working* handle. The
  synthetic fallback answers `getppid()` for one receiver — this process — and
  `Optional.empty()` for any other, which is verbatim what the interface
  specifies: *"the `Optional` is empty if the child process does not have a
  parent or if the parent is not available, possibly due to operating system
  limitations."*
* **`info()`** — minted a **zero-field** `ProcessHandle$Info`, an object
  carrying no fact about any process, which is why all six accessors had
  nothing to read. Now delegated, producing a real
  `java.lang.ProcessHandleImpl$Info` that `info0(pid)` fills in one syscall.
  The synthetic fallback mints **one** field, the pid.
* **`Info.command()`** — answered `std::env::current_exe()`. That is a real
  measurement of exactly one process and was being served from an `Info` that
  could describe any process, so for every other pid it was a fabricated command
  path dressed as a measurement. Now gated on the receiver's pid. The absence
  side is specified: *"The attributes of a process vary by operating system and
  are not available in all implementations. … The return types are
  `Optional<T>` allowing explicit tests and actions if the value is available."*
  The named consumer (Spring's
  `PathMatchingResourcePatternResolverTests$ClassPathManifestEntries`, which
  needs `current().info().command()`) is served **better** than before in
  real-JDK mode: it now gets the real `Info`, from `info0`, for the right
  process.

## 4. `Info.commandLine` — register it

**Verdict: registered, and registered empty.**

It is the sixth abstract on `ProcessHandle$Info` (`javap`: six abstract, no
default, no static) and it was registered nowhere in the tree. On a minted
receiver an abstract declaration has no `Code`, so the triple was an
`AbstractMethodError` waiting for its first caller — and
`regression-suite/src/RJdkProcess.java:128` already calls `info.commandLine()`,
so "waiting" is the only accurate word for it.

It reached nobody because `current()` already produced a real
`ProcessHandleImpl` in real-JDK mode, so no caller had ever held a minted
`Info`. **That is the fact W5-2's "the stub is not intercepting" argument rests
on, and an absence used as evidence is still an absence.**

Empty rather than measured, because a minted `Info` carries a pid and nothing
else, and `commandLine()` is `command()` and `arguments()` joined or, failing
that, *"a best-effort, platform dependent representation of the command line"* —
neither reachable from this file without reimplementing `info0`. Empty is the
specified answer for a value that is not available and is what the four siblings
beside it already answer for the same reason. In real-JDK mode `info()` returns
the real `Info` and the row is never reached.

**`hashCode()` and `equals(Object)` are deliberately still unregistered.** Both
are abstract on the interface, but an interface's `super_class` is
`java/lang/Object`, whose concrete bodies resolve first, so neither ever reaches
the abstract declaration — the same reasoning `W7-9` records for `Map$Entry`.
Registering them would be belt-and-braces at best and would break
`p60_process_handle_current`'s identity memo at worst.

## 5. The `NativeKind` misstatement

**Not one method on either interface is `ACC_NATIVE`.** Every `javap -p -v`
flags line is `ACC_PUBLIC, ACC_ABSTRACT` or `ACC_PUBLIC, ACC_STATIC`. §1.5
defines a bridge as what an `ACC_NATIVE` method *on the image* binds to, so the
ambient `Bridge` this block carried was a misstatement on **every** row.

Both halves land on `SyntheticStub`, for different reasons:

* **`current()` is a §1.4 shadow.** `acc_native: false, has_code: true`, and its
  real body is `invokestatic ProcessHandleImpl.current` — a `getstatic` of that
  class's own singleton. Static interface methods keep the native check in
  real-JDK mode, so unlike the abstract rows this one genuinely intercepts live
  pipelines, and what it intercepts is better than what it substitutes: the memo
  mints its **own** `ProcessHandleImpl` through the private `(JJ)V` constructor,
  which is not `ProcessHandleImpl.current`, so `ProcessHandle.current() !=
  ProcessHandleImpl.current()` for the rest of the run. Refused under strict,
  the real `getstatic` answers and the identity is the JDK's. Identical shape,
  reasoning and disposition to `native-io/src/process.rs`'s
  `ProcessBuilder.start()`.
* **The abstract instance rows** (11 on `ProcessHandle`, 6 on `$Info`) bind to
  no image method at all, and their entire receiver population is fabricated by
  construction. `docs/jdk-only-native-review.md`'s disposition table: *"No real
  method/class + compatibility behaviour -> CompatibilityShim (refuse under
  JdkOnly)"*, whose `NativeKind` is `SyntheticStub`.

**Why `Bridge` was not baseless, which is the part worth knowing.** In
synthetic-JDK mode these two classes have no class file and
`ClassManager::is_native_backed_jdk_stub` fabricates a carrier whose methods it
marks `MethodAccessFlags::NATIVE` (`classloading/src/class_manager.rs`). On
*that* carrier the rows really are `ACC_NATIVE`. But the carrier is not the
image, §1.5 asks about the image, and one ambient tag cannot say "true in one
mode". `SyntheticStub` is the tag that makes both modes coherent.

**Strict mode loses nothing.** After §3, no path mints a bare-interface handle
while `ProcessHandleImpl` is loadable: `current()` builds a real one, `parent()`
returns the real one `parent0` found, `native-io::build_process_handle` prefers
a real one for `Process.toHandle`. A residual mint under strict would raise
`AbstractMethodError` naming the exact triple, which is what strict mode is for
and is strictly better than a silent fabricated answer.

### The tag had to be restated at **both** registration sites

`current`, `pid` and `isAlive` are registered twice: by
`register_phase57_process` (early, via `register_essential_natives_with_shims`)
and by `register_p60_process_handle` (late, `vm_init.rs:1901` and `:2406`). The
late one wins the slot in every arm, which is what the two "keep in sync"
comments in `vm_init` are about — **but that is only true in Compatible mode.**
Under `--jdk-only` a `SyntheticStub` registration is *refused*, and every drop
arm in `NativeMethodRegistry::register` returns **before** the write to the
slot. So had the early registrar been left on `Bridge`, strict mode would have
accepted its three rows, refused the corrected rows that were meant to replace
them, and gone on dispatching exactly the bodies the re-tag exists to drop.

The general form, since this is the second way the same ambience has bitten:
**an unstated or stale re-registration does not downgrade a kind quietly; under
strict it decides it, and it decides it in the direction of whichever site is
*not* refused.** Auditing "which registrar wins" by `vm_init` order alone is
correct for the callback and wrong for the kind.

## 6. Ratchets this moves

> **STALE AS OF 2026-08-12 (later pass) — read the banner at the top.** The
> `BASELINE_SYNTHETIC_STUBS = 1038` this section is arithmetic over no longer
> exists, and the scope gap it names as a "second finding" has been closed:
> `register_boot_path` now replays `register_p60_process_handle` and
> `register_classvalue_natives`, and a source witness asserts the scope against
> `vm_init.rs`. The deltas below cannot be re-derived from today's tree; the
> reasoning about *why* a stub census cannot see a re-tag still holds.

Neither can be re-frozen from this lane — both live in files it does not own —
and neither has been run. The numbers below are counted from the source, not
measured.

**`native-builtins/tests/stub_ratchet.rs`, `BASELINE_SYNTHETIC_STUBS = 1038`,
`SLACK = 0`, assertion `synthetic <= BASELINE`. Expected: 1038 -> 1041.**
Its `register_boot_path` calls `register_essential_natives_with_shims`,
`register_concurrent_natives`, `register_forkjoin_quiescence`,
`register_stamped_lock_natives`, `register_io_natives` and
`register_collections_natives` — and **not** `register_p60_process_handle`,
which `vm_init` calls in both arms. So this census sees only the three rows in
`register_phase57_process`. Same shape as the 939 -> 1038 move on 2026-08-11
(99 `java/util/logging/` rows) and for the same reason: the ratchet counts stubs
and cannot tell a new fake from a correctly re-tagged old one.

*Second finding, not this record's defect:* **`register_boot_path` is missing
`register_p60_process_handle` (and `register_classvalue_natives`, registered
beside it at `vm_init.rs:2406`/`:2413`).** Its own doc comment says it exists
because the previous scope *"was false, and the gap was large"*. It is still
narrower than `vm_init`'s real-JDK arm by at least those two passes, so 18
registrations in this block are outside the gate entirely. Architecture §7's
"which registrars did it call" question, in a file whose job is to answer it.

**`regression-suite/bridge-ratchet.sh` /
`scripts/baselines/jdk-only-bridge-ratchet.json`** (`"mode": "compatible"`,
`slack: 0`). That census runs the real VM, so it sees all 21 rows in the block
(one row per *registration*, so the three shared triples count twice):
`bridge_without_acc_native` **8912 -> 8892** (20 rows leave `Bridge`;
`commandLine` is new and was never counted), `bridge_shadows_bytecode`
**6066 -> 6064** (only `current()` has `Code`, and it is registered twice).
`min_total_rows: 8000` is unaffected — the block gains one row.

## 7. Residuals

### 7.1 `onExit()` on a minted handle is still wrong, and is not this fix

It returns an already-completed `CompletableFuture`. The JDK specifies
`IllegalStateException` for `ProcessHandle.current().onExit()` — the process
cannot wait for itself — and for any other handle the future must complete when
that process exits. `p60_process_handle_current`'s doc comment already records
this as one of the three defects the real-`ProcessHandleImpl` promotion fixed
for `current()`, and the same promotion now covers `parent()`, `children()`,
`descendants()` and `info()`. `onExit()` was left alone because
`ProcessHandleImpl.onExit()` registers a reaper against
`ProcessHandleImpl.completions` and `waitForProcessExit0`, and delegating a
*minted* handle into that machinery changes process-reaping behaviour rather
than just an answer. It deserves its own change with its own argument.

### 7.2 The synthetic-JDK empty stream is still a fabrication

`p60_unmeasurable_process_tree` answers an empty `Stream` when there is no
`ProcessHandleImpl` to delegate to. This is stated in place rather than hidden:
in synthetic-JDK mode there is no process table this file can consult through a
supported route, and under `--jdk-only` the `SyntheticStub` tag means the
function is unreachable because the registration that would call it is refused.
Removing the last of it needs a process enumerator `native-builtins` can call
without a real JDK — i.e. the four `pub`s W3-6 asked for, or an equivalent in a
crate below `native-builtins`.

### 7.3 APPLIED 2026-08-12 — the synthetic `$Info` carrier now declares `commandLine`

**Was:** `classloading/src/class_manager.rs` fabricated the
`java/lang/ProcessHandle$Info` carrier with five methods — `command`,
`arguments`, `user`, `startInstant`, `totalCpuDuration` — and no `commandLine`.
In synthetic-JDK mode an `invokeinterface ProcessHandle$Info.commandLine()`
therefore fails at **resolution**, before native dispatch is reached, so §4's
registration was real-JDK-only.

**Now:** `synthetic_stub_ctor_methods`' `if name == "java/lang/ProcessHandle$Info"`
arm carries `mk("commandLine", "()Ljava/util/Optional;")` beside the other five,
so the arm declares all six of the image's abstracts. Both halves re-verified
before editing rather than taken from this record: the registration §4 describes
is live (`r.register(phi, "commandLine", "()Ljava/util/Optional;", p60_empty_optional)`
in `native-builtins/src/phases_late.rs`) and the carrier's list really was five
names. The `java/lang/ProcessHandle` arm above it is complete against the 13
abstracts that file registers, so nothing else there needed a row.

**It already had a scheduled witness, and that is why no new fixture is
proposed.** `regression-suite/src/RJdkStrict.java`'s `processHandleInfo` reflects
over `ProcessHandle.Info.class.getDeclaredMethods()` and asserts the sorted names
equal exactly `[arguments, command, commandLine, startInstant, totalCpuDuration,
user]` — an **exact set**, not a `contains` sweep, which is the only shape that
can see an omission. `RJdkStrict` is scheduled in `JDKONLY_CLASSES`, so on a real
JDK it reads the image and passes either way; the arm this change repairs is the
`--synthetic-jdk` **mode**, which no suite runs (README §2.6). A mirror
assertion of the same exact set was added to
`process_handle_native_fallback_has_verifier_visible_callable_shape` in
`classloading/src/class_manager.rs`, which does exercise the fabrication path
directly and is the only instrument that can go red for this today.

**No ratchet moves.** §6's two censuses count native REGISTRATIONS; this adds a
fabricated *method declaration* and no registration. The `commandLine`
registration itself was already counted when §4 landed.

## 8. What was rejected from the handed-over patch

W3-6's `## Out-of-file patch (not applied)` was a strong lead and its diagnosis
was right. Three parts of it did not survive verification:

1. **The four `pub`s in `native-io/src/process.rs`, and the bodies built on
   them.** Rejected as unnecessary, not merely as out-of-scope. Delegating to
   `ProcessHandleImpl` reaches the same probes through the JDK's own bytecode
   and cannot drift from `getProcessPids0`/`parent0` the way a second caller of
   the same helpers can. It also gets `startTime` right by construction (§3),
   which the patch's `os_parent_pid(pid)` form does not — it never had one.
2. **`p60_handle_stream`, the helper the patch says "does not exist yet".** Not
   written: with delegation there is no pid list to wrap, because
   `ProcessHandleImpl.children(long)` returns a real `Stream` of real
   `ProcessHandleImpl`s. The patch's own footnote about needing
   `STREAM_NUM_FIELDS` rather than the 1-field layout **is correct and is
   confirmed** (`native-collections/src/lib.rs`: `const STREAM_NUM_FIELDS:
   usize = 2`), and it applies to the fallback path, which now uses the `pub`
   `make_stream_from_elements` instead.
3. **`p60_empty_optional` for `commandLine` — accepted, but not for the stated
   reason.** The patch's comment calls the triple *"only unhit because the
   `info()` stub is currently unreachable from a real `ProcessHandleImpl`"* and
   cites that as W5-2's evidence. The registration is right; the framing is the
   thing this record corrects in §4.

Also rejected: the patch's deferral of `Info.command()` as *"a design decision,
not a patch"* requiring a pid slot on the synthetic `Info` or a real
`ProcessHandleImpl$Info`. Both are done — the real `Info` in the delegating
path, the pid slot in the fallback — and together they are smaller than the
deferral implied.

---

## References

`docs/architecture/natives-over-real-jdk-classes.md` (§1 registration is the
gate, §3 last-write-wins, §7 scoped censuses),
`docs/jdk-only-native-review.md` (the disposition table),
`docs/known-issues/jdk-only/W7-9-minted-interface-abstract-methods.md` (the
superclass-walk argument, and §8.2's `invoke_static` blocker corrected in §3),
`docs/known-issues/jdk-only/W3-6-processimpl-missing-natives.md`,
`docs/known-issues/jdk-only/W5-2-two-silently-skipped-process-checks.md`,
`docs/known-issues/jdk-only/W6-10-process-enumeration-syscall-cost.md`,
`docs/known-issues/jdk-only/W7-5-registrars-that-never-shipped.md`.
