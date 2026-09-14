# G21-1 — the `Handler.setLevel` store, and how far `RJdkIntrinsics3` got

**Status:** MIXED. The **before is MEASURED** on both VMs in both modes; the two
fixes are written in `native-builtins/src/phases_early.rs`; their **after is
PREDICTED**, because this lane was forbidden to build (§8) and the binary
predates the edit by construction. **Where the vector goes next is MEASURED**,
not predicted, and that is the unusual part of this record: `RJdkIntrinsics3`
takes a `--only=<family>` argument, so every family *behind* the blocker was run
on the current binary and the vector's post-fix stopping point is a measurement
rather than a guess (§5).

**Provenance:** MEAS on both VMs. Oracle: HotSpot 25.0.3+9-LTS
(`C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot`). VM under test:
`C:/craton/target-fcheck/release/cratonvm.exe`, built from `d2e127930`,
`--java-home` at that JDK, `CRATONVM_DISABLE_DEFAULT_WATCHDOG=1`. Probes:
`scratchpad/g21/{G21Probe,G21Tail}.java` — 34 rows and the `logrec` tail, no
lambdas, no `invokedynamic` (compiled `-XDstringConcat=inline`, so nothing here
can fail on `LambdaMetafactory` or `StringConcatFactory`).

This lane owns exactly one file, `native-builtins/src/phases_early.rs`.
Everything else is a nomination (§7).

---

## 0. The headline

| row / vector | before (MEASURED) | after |
|---|---|---|
| `Handler.setLevel(null)` `--jdk-only` | **RETURNED**; oracle throws a bare `NullPointerException` | PREDICTED: bare NPE, no store |
| `Handler.getLevel()` after that call, `--jdk-only` | **`null`**; oracle still answers `ALL` | PREDICTED: `ALL` |
| `Formatter.formatMessage(null)` **both modes** | **returned `""`**; oracle throws the `record` helpful-NPE | PREDICTED: that NPE, verbatim |
| JUL null axis, `--jdk-only` | **3 of 34 rows wrong — all three in this file** | PREDICTED 0 of 34 |
| JUL null axis, Compatible | **3 of 34 wrong** — one of them this file's | PREDICTED 2 of 34; the other two are §7 N1/N2 |
| `RJdkIntrinsics3` `--jdk-only` | RED at `logrec:Handler.setLevel(null)`, last family line `bigint=38`, **741 checks** | PREDICTED **800 checks**, `logrec=35` printed, RED at the first `tlocal` ITL row — §5, and that destination is MEASURED |
| `RJdkLogging` `--jdk-only` / Compatible | **GREEN, 79 checks** both | unchanged; blast radius measured empty (§6.3) |
| `RJdkHello` `--jdk-only` / Compatible | **GREEN, 41 checks** both | unchanged |

The oracle passes `RJdkIntrinsics3` with **1011 checks**.

---

## 1. The oracle asymmetry, re-verified from scratch

G15-1 §2 recorded this axis and warned that it punishes generalisation. It was
**not taken on trust**: `scratchpad/g21/G21Probe.java` was written
independently, run on HotSpot 25.0.3+9-LTS, and every one of its 34 rows agrees
with G15-1. The table below is this lane's own transcript, not a copy.

### 1.1 `Handler` — four sibling setters, three verdicts

| call | HotSpot | CratonVM `--jdk-only` | Compatible |
|---|---|---|---|
| `setLevel(null)` | `NullPointerException` msg=`null` | **RETURNED** | = |
| `setFormatter(null)` | `NullPointerException` msg=`null` | = | = |
| `setErrorManager(null)` | `NullPointerException` msg=`null` | = | = |
| `setFilter(null)` | **RETURNED** | = | = |
| `setEncoding(null)` | **RETURNED** | = | = |
| `setEncoding("bogus-xyz")` | `UnsupportedEncodingException: bogus-xyz` | = | = |

and the two **state** rows, which are the ones a "throw after the store" fix
still gets wrong:

| call | HotSpot | `--jdk-only` |
|---|---|---|
| `getLevel()` after a REFUSED `setLevel(null)` | `ALL` | **`<null>`** |
| `getFormatter()` after a REFUSED `setFormatter(null)` | `SimpleFormatter` | = |

`setFormatter` is the control: the same shape, already correct, and it proves
the expected behaviour is *refuse before mutating* rather than *refuse*.

### 1.2 `Logger` — the same-named setter goes the other way

| call | HotSpot | `--jdk-only` | Compatible |
|---|---|---|---|
| `setLevel(null)` | **RETURNED** | = | = |
| `setFilter(null)` | **RETURNED** | = | = |
| `setParent(null)` | `NullPointerException` msg=`null` | = | **RETURNED** |
| `addHandler(null)` | `NullPointerException` msg=`null` | = | = |
| `removeHandler(null)` | **RETURNED** | = | = |
| `logp(null, c, m, msg)` | `NullPointerException: Cannot invoke "java.util.logging.Level.intValue()" because "level" is null` | = | **RETURNED** |
| `logp(SEVERE, null, null, msg)` | **RETURNED** | = | = |

`setLevel` throws on `Handler` and returns on `Logger`. `addHandler` throws and
`removeHandler` returns, on the same receiver, with the same argument. And
inside the **one** `logp(Level,String,String,String)` signature, the `Level`
null is fatal while both source-name nulls are ordinary.

### 1.3 `LogRecord` — nine return, two throw

| call | HotSpot | `--jdk-only` | Compatible |
|---|---|---|---|
| `setLevel(null)` | `NullPointerException` msg=`null` | = | = |
| `setInstant(null)` | `NullPointerException: Cannot invoke "java.time.Instant.toEpochMilli()" because "instant" is null` | = | = |
| `setLoggerName` / `setMessage` / `setParameters` / `setThrown` / `setSourceClassName` / `setSourceMethodName` / `setResourceBundle` / `setResourceBundleName` (null) | **all RETURN** | = | = |
| `getLevel()` after a REFUSED `setLevel(null)` | `INFO` | = | = |

### 1.4 `Formatter`

| call | HotSpot | `--jdk-only` | Compatible |
|---|---|---|---|
| `formatMessage(null)` | `NullPointerException: Cannot invoke "java.util.logging.LogRecord.getMessage()" because "record" is null` | **returned `""`** | **returned `""`** |
| `formatMessage(record "one={0} two={1}", params A,B)` | `one=A two=B` | = | = |
| `formatMessage(record with null message)` | `null` | = | = |

### 1.5 The whole measured divergence

**`--jdk-only`: 3 rows of 34, and all three are bodies in this file.**
**Compatible: 3 rows of 34** — `Logger.setParent(null)` (`lib.rs`),
`Logger.logp(null,…)` (`logmanager.rs`; a sibling lane's uncommitted fix targets
it and could not be measured), and `Formatter.formatMessage(null)`, which is
this file's.

---

## 2. Which body actually runs — the registry dump, and a correction to G15-1 §7

`--dump-native-registry`, placed **before** the main class (after it the flag is
silently ignored: exit 0, no file) and given a **Windows** path (Git Bash's
`/tmp` is a POSIX mapping the VM cannot resolve):

```
--jdk-only, dumped against G21Probe (which calls both triples):

java/util/logging/Handler  setLevel  (Ljava/util/logging/Level;)V
    kind=intrinsic  owns_slot=true  overwrote=null  invocations=12
    registered_by=native-builtins/src/phases_early.rs:21102
java/util/logging/Formatter  formatMessage  (Ljava/util/logging/LogRecord;)Ljava/lang/String;
    kind=bridge     owns_slot=true  overwrote=null  invocations=2
    registered_by=native-builtins/src/phases_early.rs:21189
```

One registration per triple, `overwrote=null`, owning the slot, demonstrably
running. In **Compatible** the same dump shows this file's `setLevel` at
`owns_slot=false invocations=0` and `reflect_annotations.rs:370` at
`owns_slot=true invocations=11` — that registrar already carries the correct
contract, which is why Compatible mode gets rows 1 and 2 right and `--jdk-only`
does not. `--jdk-only` never runs `register_synthetic_overrides`.

`java/util/logging/Handler.setLevel` **is** listed in `RETIRED_SHADOW_TRIPLES`
(`native-api/src/retired_shadow.rs:445`) and the retirement does not reach it:
that retag fires only on an effective category of `Bridge`, and this
registration sits under the function's ambient `Intrinsic`. Reading the
retirement table would have said this row was already gone.

**Correction to G15-1 §7.** That record states the dump is taken at registration
time and that *"every `java/util/logging` row reads `invocations=0`"*, so
`invocations` is not usable as evidence. On schema_version 4 it **is** usable,
and it is workload-dependent exactly as expected. Same triple, same binary,
three workloads:

| workload | `Handler.setLevel` | `Formatter.formatMessage` | `owns_slot` |
|---|---|---|---|
| `RJdkHello` | `invocations=0` | `invocations=0` | `true` |
| `RJdkIntrinsics3` | `invocations=2` | `invocations=0` | `true` |
| `G21Probe` | `invocations=12` | `invocations=2` | `true` |

`owns_slot` is constant across all three and is the workload-independent proof
of ownership; `invocations=0` on the `RJdkHello` row is a statement about
`RJdkHello`, not about the registration. Dump against a workload that exercises
your triple.

---

## 3. The fix — two rows, both in `phases_early.rs`

### 3.1 `Handler.setLevel` — check BEFORE the store

The body was four lines and wrote raw slot 0 unconditionally:

```rust
let this = obj_arg(args, 0)?;
ctx.set_field(this, 0, args[1]);
Ok(Some(Value::Object(None)))
```

Two of `RJdkIntrinsics3`'s `logrec` assertions ride on it, and they fail for
*different* reasons: the first because nothing throws, the second because the
null was already stored by the time anything could throw. A guard placed after
the store fixes one and leaves the other. The guard is now the first statement
after the receiver is resolved, and the NPE is **bare** — `getMessage()` is
`null`, matching `Handler.setFormatter(null)` and `LogRecord.setLevel(null)`.
No message was invented; `Level.parse(null)` shipped the right *class* with a
fabricated `"Name cannot be null"` for months (G15-1 §4), which is why the unit
tests assert the message and not just the class.

### 3.2 `Formatter.formatMessage(null)` — the comment that had gone stale

The null-record arm returned the empty string, under a comment reading *"nothing
measured exercises the null, so the historical answer is kept rather than
introducing a throw that no test can adjudicate."* §1.4 is that measurement. It
now throws HotSpot's helpful NPE, transcribed into a named constant
(`JUL_NPE_NULL_FORMAT_RECORD`, `phases_early.rs:21346`) so the string has one
home. This row is wrong in **both** modes, and this registration owns the slot
in both, so one edit fixes both.

Note the two rows do **not** share a shape: `Handler.setLevel` is bare, and
`formatMessage` is a helpful-NPE naming the `record` parameter. Same family,
same file, same commit, two different answers — asserted verbatim for that
reason.

---

## 4. Nothing else in this file has the defect — measured, not assumed

The brief asked whether any other JUL row here has the same store-then-return
shape. **Ten do, and all ten are correct.** `lr_set` is the shared
store-then-return body behind `LogRecord.set{Message,LoggerName,Parameters,`
`Thrown,ResourceBundle,ResourceBundleName,SourceClassName,SourceMethodName,`
`Millis,SequenceNumber,ThreadID}`, and §1.3 measures every reference-typed one
of them as RETURNING on HotSpot. The store-then-return shape is the *correct*
implementation there. `Handler.setLevel` was the one row where it was wrong.

`LogRecord.setLevel` and `setInstant` — the two that DO throw — are **not
registered by this file at all** (confirmed in the `--jdk-only` dump: `getLevel`
is there, `setLevel` is not). The real bytecode runs and throws on its own, and
§1.3 measures both as already matching. Completing the set by adding a raw-slot
`LogRecord.setLevel` here is exactly how `Handler`'s unguarded body came to
exist; a unit test now pins the absence.

Likewise this file registers **none** of `Handler.setFilter`, `setEncoding`,
`setFormatter`, `setErrorManager`. All four are served by real bytecode under
`--jdk-only` and all four are measured correct. Two of them must RETURN.

---

## 5. How far `RJdkIntrinsics3` gets — MEASURED, not predicted

`RJdkIntrinsics3.main` accepts `--only=<family>` (and `--list`). Because a
family runs standalone, **every family behind the blocker was run on the current
binary**, which turns "where does the vector go after this fix" from a
prediction into a measurement:

| family | oracle | CratonVM `--jdk-only`, run standalone |
|---|---|---|
| `logrec` | 35 | RED at `Handler.setLevel(null)` — the two rows fixed here |
| `tlocal` | 26 | **RED**: `tlocal:ITL value is copied at CONSTRUCTION, not read live: expected "parent-init", got "set-after-construction"` |
| `fmtobj` | 22 | RED: `new Formatter().out() is a StringBuilder: expected true, got false` |
| `inet` | 34 | RED: `createUnresolved(null, 80): expected IllegalArgumentException, got NullPointerException: null object argument` |
| `regex` | 42 | **GREEN, 42** |
| `misc` | 21 | RED: `new String(builder holding U+DC00).charAt(0): expected 56320, got 65533` |
| `bufslice` | 33 | RED: `BE get() at the limit: expected BufferUnderflowException, got IndexOutOfBoundsException` |
| `mathexact` | 57 | **GREEN, 57** |

The `logrec` tail past the blocker was measured separately
(`scratchpad/g21/G21Tail.java`, which replays the vector's own lines 1799-1822
with the failing assertion swallowed):

```
                                       HotSpot        CratonVM --jdk-only
getLevel after setLevel(WARNING)       WARNING        WARNING
getLevel after setLevel(ALL)           ALL            ALL
getLevel after setLevel(null)          ALL            <null>      <- fixed here
flush + close + close again            none           none        <- already green
```

So the two rows fixed here are the **only** red rows left in `logrec`, and the
family will complete at 35.

**Therefore, PREDICTED from MEASURED parts:** after this fix
`RJdkIntrinsics3 --jdk-only` prints `CK RJdkIntrinsics3 logrec=35` as its last
family line and dies at `tlocal` check 25 of 26 — the `InheritableThreadLocal`
capture-timing row — having passed **800** checks of 1011
(741 through `bigint` + 35 `logrec` + 24 `tlocal`).

That next divergence is **already known and deliberately untouched**:
HANDOFF-20260814 §6.5 and F41-1 §6 record `InheritableThreadLocal` capture
timing as a documented workaround protecting every `Executors` path, and commit
`37263df59` recorded the ITL timing divergence rather than fixing it. Whoever
takes `RJdkIntrinsics3` next should read that before touching it, and should
know that four more families behind it are red for four unrelated reasons.

---

## 6. The trap, and the in-tree caller that proves it is not theoretical

### 6.1 A blanket rule breaks 22 measured rows

Any rule of the form "JUL setters reject null" breaks `Handler.setFilter`,
`Handler.setEncoding`, `Logger.setLevel`, `Logger.setFilter`,
`Logger.removeHandler`, the two source-name parameters of `logp`, and nine
`LogRecord` setters. Any rule of the form "JUL setters tolerate null" is what
Compatible mode used to be, and G15-1 measured it as 17 wrong rows.

### 6.2 CratonVM's own code calls `setLevel(null)` on purpose

`native-builtins/src/jmx.rs:4238` implements
`LoggingMXBean.setLoggerLevel(String, String)` and, when the level name is null
or empty, invokes `Logger.setLevel` with `Value::Object(None)` — deliberately,
with the comment *"A null/empty level name clears the logger's level
(inherit), matching the MXBean contract."* A guard written as "`setLevel`
rejects null", applied by name rather than by class, would have broken a
shipping MXBean path inside this repository. This is the trap with a caller
attached: the same *method name* on the sibling class must keep accepting the
same value.

The only other in-tree invoker, `logmanager.rs:1818`, calls
`Handler.setLevel` strictly inside `if let Some(level) = resolve_standard_level(…)`,
so it can never reach the new guard.

### 6.3 Blast radius across the regression suite

`grep` over `regression-suite/src`: `setLevel(null)` appears **only** in
`RJdkIntrinsics3.java:1808`, and `formatMessage(null)` appears nowhere.
`RJdkLogging.java` calls `setLevel` nine times, always with a real `Level`, and
`formatMessage` once with a real record (line 503) — all on the unchanged paths.
`RJdkLogging` and `RJdkHello` were measured GREEN in **both** modes on the
current binary before the edit (79 and 41 checks) and are the re-run gate.

---

## 7. NOMINATIONS — outside this lane's one file

**N1 — `native-builtins/src/lib.rs`, `java/util/logging/Logger.setParent`.**
Compatible mode RETURNS on null; HotSpot throws a bare `NullPointerException`.
MEASURED here (§1.2) and independently by G15-1 (its N4, which cites
`lib.rs:18035`). `--jdk-only` is already correct, so this is Compatible-only.

**N2 — `native-builtins/src/logmanager.rs`, the null-`Level` gate on
`Logger.logp` and the `log` family.** Compatible mode RETURNS where HotSpot
throws `Cannot invoke "java.util.logging.Level.intValue()" because "level" is
null`. G15-1 §4 reports a fix for exactly this already written into
`logmanager.rs`, unmeasured; **this lane measured the row still red on the
current binary**, which is expected (the binary predates that edit) and is
recorded so the next lane knows the row's last measured state rather than its
predicted one. Re-measure after the next build before believing either.

**N3 — `native-collections`, `ConcurrentHashMap`'s null-key NPE message.**
Unchanged from G15-1 N3 and not re-measured here.

**N4 — the `--only=<family>` flag is under-used.** `RJdkIntrinsics3` (and,
worth checking, its siblings) accepts `--only=`, `--list` and `--measure`.
Every lane that has reported "the vector is red at assertion X and we cannot
see past it" could have measured the whole tail in one loop, on the binary it
already had, without building. That is how §5 exists. It costs one `for` loop
and it converts a lane's biggest guess into a table.

---

## 8. What this lane did NOT settle

* **It did not measure its own "after".** `cargo build` / `check` / `test` were
  forbidden (the orchestrator holds the target-dir lock), so the binary cannot
  contain these edits. Every "after" in §0 is PREDICTED. What is *not*
  predicted is §5's destination, which was measured on families the edit does
  not touch, and §1's before, which is measured throughout.
* **The `Handler.setLevel` prediction is unusually strong, and should still be
  re-measured.** The identical contract — check before store, bare NPE — is
  already running on the identical triple in Compatible mode
  (`reflect_annotations.rs:370`), and §1.1 measures it there as matching HotSpot
  on both rows. So the after is predicted from a *measured equivalent* rather
  than from source alone. `formatMessage` has no such equivalent: it is wrong in
  both modes, and its after is predicted from source only.
* **It did not run the unit tests it added.** They are written against the
  registry-lookup idiom already used by `http2.rs`'s `find_cb` and
  `logmanager.rs`'s `assert_npe`, but no lane can claim a passing test it did
  not run.
* **It did not touch `FileHandler` or `SocketHandler`.** G15-1 §1.1 has their
  HotSpot rows; neither was run on CratonVM by either lane.
* **It did not investigate the four other red families in §5.** They are
  measured, named, and left for whoever takes the vector next.
* **It did not adjudicate `Handler.getErrorManager()` on a fresh handler**
  (G15-1 N7), which is a `reflect_annotations.rs` row.

---

## 9. Verification performed

* `rustfmt --edition 2021 --check`, run **in place, in its crate**: **35 diff
  hunks**, and the baseline (`git show HEAD:native-builtins/src/phases_early.rs`
  written to a temp file and checked the same way) is **also 35**. Zero new
  deviations against the file's pre-existing formatting debt. The baseline was
  obtained **without** `git stash` — G15-1 §7 discloses a lane that used
  `git stash push`/`pop` for this and had to report it; `git show` needs no
  write and no state change. No state-changing git command was run by this lane.
* Zero CR bytes (`tr -cd '\r' < file | wc -c` → 0). The file is LF-only and
  stays that way.
* Every edited region re-read in full context; no duplicate `fn` names
  introduced (the one duplicate in the file, `make_byte_array`, is pre-existing
  and lives in two different scopes).
* Re-running `RJdkIntrinsics3`, `RJdkLogging` and `RJdkHello` against a binary
  containing the fix is the outstanding gate. Their pre-edit state is in §0 so
  the comparison is available to whoever builds next.
