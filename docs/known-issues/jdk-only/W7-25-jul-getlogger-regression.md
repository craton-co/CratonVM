# W7-25 — the JUL `getLogger` regression: the cause was not the one we recorded

> **RECONCILED 2026-08-12 (W7-55-record-reconciliation.md) — BOTH "not applied"
> OUT-OF-FILE PATCHES ARE APPLIED, AND MOST OF SECTION 7's OPEN TABLE IS NOW
> CLOSED.**
>
> * **6.1** (move the `LogManager` `getLogManager`/`getLogger` pair into
>   `r.with_category(NativeKind::Bridge, ...)`) — **APPLIED**, commit
>   `4eaa5d321`, widened by `3b20b83b5`. Site:
>   `native-builtins/src/phases_early.rs:20627-20637`.
> * **6.2** (schedule `RJdkLogging` in `JDKONLY_CLASSES`) — **APPLIED**,
>   `regression-suite/run.sh:110`.
> * Section 7's table, re-adjudicated: `getLogManager()` fresh-manager —
>   closed by 6.1. `setUseParentHandlers(false)` — **closed**, commit
>   `ac7c5a9cd`, `native-builtins/src/logmanager.rs:4206-4226` reading
>   `jul_logger_use_parent_handlers_table()` (`native-builtins/src/lib.rs:359`).
>   `Formatter.formatMessage` leaving a raw `{0}` — **closed in both modes**,
>   commits `a7bed655d` and `8eb60d3c1`, real body at
>   `native-builtins/src/phases_early.rs:20556-20720`. Missing inferred source
>   class/method — **closed in Compatible only**, commit `b7a8c37bf`,
>   `stamp_inferred_caller` at `logmanager.rs:4168`; the strict half is its own
>   record, jul-logrecord-infercaller-is-inert-under-jdk-only-20260812.md.
> * **Residual: STILL OPEN** — the `Supplier` convenience overloads evaluate a
>   suppressed supplier; `LogManager.getLogger` demand-creates for an undemanded
>   name; `log(LogRecord)` is not level-gated.
> * **Context for anyone re-measuring:** on 2026-08-12 `RJdkLogging` failed in
>   Compatible mode on a **control** binary pre-dating that day's merges, with
>   identical errors. Its Compatible-mode failure is pre-existing, not a
>   regression from the JUL work above.

**Status:** root cause **found and measured**; the fix is a two-line category
change in a file this lane does not own (§6.1, not applied). Two adjacent
`Compatible`-mode defects **fixed** in this lane's files. The missing vector,
`regression-suite/src/RJdkLogging.java`, **landed** — 63 checks, measured on
HotSpot 25.0.3+9 first.

W7-22 §4 found this regression and named a cause. That cause is wrong, and the
repair it prescribed — "build the singleton through its real constructor" — was
written here, measured to be **inert in both modes**, and reverted. The real
mechanism is more general and worth more than the bug: **a retired shadow is
silently reinstated by any other registrar holding the same triple under a
`NativeKind` the retirement is exempt from.**

Binary for every measurement below: `target/release/cratonvm.exe` built
2026-08-11 19:41, against `Eclipse Adoptium jdk-25.0.3.9-hotspot`, HotSpot
25.0.3+9 as the oracle on every arm. Nothing was rebuilt; §8 separates what was
run from what is reasoned.

---

## 0. Reproduction, and the mode split

One probe, three arms, same classpath:

```java
LogManager.getLogManager();                 // 1
Logger.getLogger("repro.x");                // 2
Logger.getGlobal();                         // 3
// 4: a logger with a Handler of its own, then info/warning/fine/finest
//    and log(Level.INFO, () -> "sup")
```

| arm | 1 | 2 | 3 | 4 |
|---|---|---|---|---|
| HotSpot 25.0.3 | ok | ok | ok | `[INFO:i, WARNING:w, FINE:f, INFO:sup]` |
| CratonVM `--real-jdk` | ok | ok | ok | `[INFO:i, WARNING:w, FINE:f]` — **`sup` missing** |
| CratonVM `--jdk-only` | ok | **NPE** | ok | **NPE** |

```text
java.lang.NullPointerException: Cannot invoke
  "java.util.logging.LogManager$LoggerContext.demandLogger(String, String, java.lang.Module)"
  because the return value of "java.util.logging.LogManager.getSystemContext()" is null
```

So: the strict-mode NPE W7-22 reported, reproduced exactly, plus its §4.1
`Compatible`-mode `log(Level, Supplier)` drop, reproduced exactly. Note arm 1 —
`getLogManager()` **succeeds** in strict. That is the thread that unpicks this.

---

## 1. The cause: a refusal does not remove what it would have overwritten

`--dump-native-registry` schema 4 carries `registered_by` and `overwrote` per
row, which is what settles this. Three registrars hold
`java/util/logging/LogManager.getLogManager()Ljava/util/logging/LogManager;`:

| registrar | `Compatible` | `--jdk-only` |
|---|---|---|
| `phases_early.rs:20272` | `intrinsic`, overwrote nothing | **`intrinsic` — SURVIVES, and now wins** |
| `lib.rs:16964` | `synthetic-stub`, overwrote `intrinsic` | refused |
| `logmanager.rs:5472` | `synthetic-stub`, overwrote `synthetic-stub`, **wins** | refused |

In `Compatible` the last registration wins, and it is `logmanager.rs`'s — the
one that caches a singleton through `ensure_singleton`. Under `--jdk-only` the
retirement refuses `logmanager.rs`'s and `lib.rs`'s, because both are `Bridge`s
over real bytecode. **A refusal is not a removal**: it leaves the registration
it was going to overwrite standing. What is left standing is
`phases_early.rs`'s, whose body is

```rust
let obj = try_alloc_concurrent_synthetic(ctx, "java/util/logging/LogManager", 0)?;
Ok(Some(Value::Object(Some(obj))))
```

— no cache, no `<init>`, no singleton. Measured, three successive
`getLogManager()` calls in one process:

| | call 1 | call 2 | call 3 |
|---|---|---|---|
| HotSpot | idh 2016447921 | same | same |
| CratonVM `--real-jdk` | idh 2 | same | same |
| CratonVM `--jdk-only` | **idh 7** | **idh 8** | **idh 9** |

A fresh, unconstructed `LogManager` per call. Its `systemContext` is null, and
that null is the NPE. Identical under `--nojit`, so dispatch caching is not the
mechanism.

### Why the retirement could not see it

`register_phase54_logging_extras` opens with

```rust
r.set_category(cratonvm_native_api::NativeKind::Intrinsic);
```

and closes 357 lines later with `r.set_category(__prev_cat)`. The category is
**ambient**, so every registration in between inherits `Intrinsic` — including
the two `LogManager` fabricators at the very end, which are not intrinsics by
any reading (an intrinsic cannot give an answer the bytecode would not; these
give an answer the bytecode never would). `Intrinsic` is exempt from the shadow
retirement. So the retirement re-tagged the two honest `Bridge` rows and could
not touch the one that was mislabelled.

**Generalise this, because it is not about logging.** A shadow retirement is
only as complete as the set of registrars it can see. Before any future wave is
called verdict-neutral, take the census in BOTH modes and diff the surviving
rows per triple — a triple that still has a registrar under `--jdk-only` after
its `Bridge` was retired has not been retired at all, it has been handed to
whoever else was holding it. `overwrote` is the field that shows the chain.

---

## 2. The state the retirement wanted is already real

W7-22 §4's blocked list says `LogManager` must be "built by its real
constructor" before `Logger.getLogger` can run as bytecode. It already is.
`LogManager.<init>()V` is itself a retired shadow, so under `--jdk-only` the
real ctor runs, and the static `LogManager.manager` — the object real
`getLogManager()` bytecode returns — comes out built. Measured with
`--add-opens java.logging/java.util.logging=ALL-UNNAMED`, reflecting the
**static field**, not the value `getLogManager()` handed back:

| field | HotSpot | CratonVM `--jdk-only` | CratonVM `--real-jdk` |
|---|---|---|---|
| `props` | `Properties` | `Properties` | null |
| `systemContext` | `SystemLoggerContext` | `SystemLoggerContext` | null |
| `userContext` | `LoggerContext` | `LoggerContext` | null |
| `configurationLock` | `ReentrantLock` | `ReentrantLock` | null |
| `closeOnResetLoggers` | `CopyOnWriteArrayList` | `CopyOnWriteArrayList` | null |
| `listeners` | `SynchronizedMap` | `SynchronizedMap` | null |
| `loggerRefQueue` | `ReferenceQueue` | `ReferenceQueue` | null |
| `rootLogger` | `RootLogger` | **null** | null |

Seven of the eight. `Compatible`'s column is all-null for the same reason it
does not matter there: `<init>` resolves to `native_jboss_init`, a bare
`Ok(None)`, and every accessor is a native anyway.

**`rootLogger` being null is not a residual.** Read off
`javap -p -c --module java.logging`: the JDK writes it only from
`ensureLogManagerInitialized`, which returns immediately unless
`initializationDone` is set, and every consumer on the `getLogger` path is
guarded for a null — `LoggerContext.requiresDefaultLoggers()` gates
`ensureInitialized`/`ensureAllDefaultLoggers`; `ensureDefaultLogger` branches on
`arg == null` to a bare `return` with the `AssertionError` behind
`$assertionsDisabled`; `processParentHandlers` only compares against it with
`if_acmpeq`. HotSpot itself hands out a null `rootLogger` to any `LogManager`
that is not the static one.

**So the correct fix builds no state. It removes a shadow.**

---

## 3. Why both dispositions W7-22 offered are out of reach from this lane

W7-22 §4 offers two, and this lane owns `native-builtins/src/logmanager.rs` and
`native-builtins/src/logging_shims.rs`:

* **Hold the four triples back** — `native-api/src/retired_shadow.rs`. Not
  owned. It would also work by *undoing* the retirement on `Logger.getLogger`,
  which puts the native back in front of the bytecode; that restores the score
  without restoring the property the retirement was for.
* **Build the singleton through its real constructor** — this is
  `allocate_log_manager`, which IS owned, and it was written: pin the receiver,
  `ctx.invoke(CLS_JUL_LOG_MANAGER, "<init>", "()V", …)`, re-derive, swallow a
  throw back to the old null shape. Then measured, and it is **inert in both
  modes**:
  * under `--jdk-only`, `allocate_log_manager` is never reached — §1, the
    winning registrar is `phases_early.rs`'s, which does not call it;
  * under `Compatible`, `LogManager.<init>()V` is a retired shadow, and a
    retired shadow still *dispatches* there — to `native_jboss_init`, which
    writes nothing.

  It was reverted rather than landed. A change that moves nothing, landed with
  a commit message that says "fix", is the thing this campaign keeps
  cataloguing. `allocate_log_manager`'s doc comment now carries the
  measurement instead of the wrong prescription.

The fix that does work is §6.1, and it is two lines in `phases_early.rs`.

---

## 4. Fixed here: the `Supplier` log overloads

Two defects, both `Compatible`-visible, both in `logmanager.rs`, both measured
against HotSpot. W7-22 §4.1 found the first half.

**Half one — the record reached no handler.** With an application `Handler`
installed at `INFO`:

```text
HotSpot          [INFO:i, WARNING:w, SEVERE:s, FINE:f, INFO:L, INFO:sup]
CratonVM compat  [INFO:i, WARNING:w, SEVERE:s, FINE:f, INFO:L]
```

`native_jul_logger_log_supplier` resolved the supplier and called
`emit_framework_log` — the console sink — without the handler fan-out its seven
sibling overloads do.

**Half two, not previously found, and the worse one — the level was never
consulted, so the supplier ALWAYS ran.** Logger at `INFO`, counting supplier
invocations:

| call | HotSpot | CratonVM `--real-jdk` |
|---|---|---|
| `log(Level.FINEST, sup)` | 0 | **1**, and emitted |
| `log(Level.FINEST, thrown, sup)` | 0 | **1**, and emitted |
| `finest(sup)` | 0 | **1**, and emitted |

Not evaluating a suppressed supplier is the entire reason the overload exists;
a caller whose supplier costs something paid it on every suppressed call. The
`(Level, String)` sibling has always gated correctly, so this was an
inconsistency inside one family.

**The fix** routes both owned natives —`native_jul_logger_log_supplier` and
`native_jul_logger_log_throwable` — through the two pieces that already
existed: `native_jul_logger_is_loggable` (the threshold walk the siblings use,
60 lines of `Level`-shape fallbacks that must not be restated) as a gate
**before** the supplier is resolved, then `jul_log_parameterized` (publish, and
fall back to the console only if no handler took it).

**`Compatible` effect, per case, because this is a `Compatible` behaviour
change:**

| case | before | after |
|---|---|---|
| handler-less logger, admitted level | console line `{tag} [{name}] {text}` | **identical** — `jul_log_parameterized`'s fallback formats the same string |
| logger with a handler, admitted level | console line only | record delivered, console line suppressed — what the other seven already do |
| any logger, filtered-out level | supplier evaluated, console line emitted | nothing evaluated, nothing emitted |

`log(Level, String, Throwable)` rides the same native as the throwable-supplier
overloads and gains the same gate. **Watch this one on the SB/Tomcat suites:**
it is the overload frameworks actually use, and a `FINE`/`FINER` diagnostic
through it that used to reach the console will now be suppressed at the default
`INFO` threshold. That is HotSpot's behaviour and the `(Level, String)`
sibling's, but it is a visible change to console output and this lane could not
run those suites.

`finest(Supplier)` and its `info`/`warning`/`fine`/`severe` siblings are
registered from `native-builtins/src/lib.rs:17076-17106` — **not owned, not
fixed**, and still evaluate a suppressed supplier.

---

## 5. The vector: `regression-suite/src/RJdkLogging.java`

63 checks, `PASS RJdkLogging (63 checks)` on HotSpot 25.0.3+9, byte-identical
over three runs. It exists because W7-22 §4 checked rather than assumed the
coverage claim and found `grep -l "java.util.logging\|Logger.getLogger"
regression-suite/src/*.java` matching **nothing** across all 57 vectors. The 84
triples were retired on a verdict from a corpus that never called the class.

Sections, each ending in a `CK RJdkLogging …` line: the `LogManager` singleton
and its registry round-trip; `getLogger` identity and the dotted parent chain;
level filtering; the handler chain; `useParentHandlers` delivery; the `Supplier`
overloads; `LogRecord` payloads and `Formatter` substitution; formatted output
bytes; handler-level gating.

**Written against the two hazards this campaign has earned.** Every check is on
a value that came back — a captured `LogRecord`, a handler array's length, an
object identity, a byte string — never on "no exception was thrown". And
`formattedOutputIsRealBytes` drives a real `StreamHandler` over a real
`SimpleFormatter` into a `ByteArrayOutputStream` and asserts on the content,
because this subsystem fails **silently**: `PrintStream.writeln`'s own exception
table catches the `IOException` `ensureOpen()` raises on a null `out` and sets
`trouble`, so a retired `PrintStream` shadow over `System.out` discards output
and exits 0 (W7-22 §3). A vector asserting only that the call returned would
read green against a VM that logged nothing.

One draft check was **wrong and HotSpot caught it**, which is the argument for
measuring first: `SimpleFormatter`'s default pattern renders the inferred
source class and method, not the logger name. The check now asserts
`"RJdkLogging formattedOutputIsRealBytes"`, which is strictly stronger — a
record that arrived without its inferred source pair renders `null null`.

### Weakest checks, named

* `check(a instanceof LogManager, …)` on the singleton — nearly free, since the
  static type already says so. Kept only as a shape guard next to the identity
  check that carries the section.
* `hops < 32` on the parent walk — a cycle guard, not an assertion about the
  VM. The two checks either side of it (`hops > 0`, root name `""`) are the
  load-bearing ones.
* `check(!text.isEmpty(), …)` in `formattedOutputIsRealBytes` — subsumed by the
  four `contains` checks after it. It is there so a silent sink reports "the
  sink is silent" rather than a confusing content mismatch.
* `c.setLevel`/`c.getLevel` on the bare `Capture` handler — an accessor
  agreeing with itself. The `StreamHandler` arm below it is what actually
  proves the handler level is a second gate.

### Deliberately NOT asserted

HotSpot returns `null` from `LogManager.getLogger` for a name nobody demanded;
CratonVM `Compatible` returns a fresh `Logger` (measured). That is a third,
pre-existing defect — `LogManager.getLogger(String)` is an `Intrinsic` that
demand-creates through a Rust-side name registry, and the JULI/Tomcat shims are
built on that demand-creation. Asserting it would make this vector red for
something it is not gating. Recorded here instead.

### Divergence census, all 63 checks, soft-failure build

Run with `check` collecting instead of throwing, so one failure does not hide
the rest — the "list every unadmitted opcode before clearing one" discipline.

| arm | reachable | failures |
|---|---|---|
| HotSpot 25.0.3 | 63 | **0** |
| CratonVM `--real-jdk` | 63 | 9 |
| CratonVM `--jdk-only` | **3** | 10 — 8 of 9 sections die on the `getSystemContext` NPE |

The nine `Compatible` failures, and their disposition:

| # | check | cause | disposition |
|---|---|---|---|
| 39 | `useParentHandlers=false` must stop the parent walk | `setUseParentHandlers` records in `jul_logger_use_parent_handlers_table` (`lib.rs`) while the publication walk's `jul_use_parent_handlers` reads `config.useParentHandlers`, which is null on a synthetic logger — a write and a read that never meet | **open**, §7 |
| 41,42,43 | `log(Level, Supplier)` delivery and gating | §4 | **fixed** |
| 44 | `info`/`warning`/`fine`/`finest`(Supplier) | `lib.rs` registrations, not owned | **open**, §7 |
| 54 | `Formatter.formatMessage` must substitute `{0}` | the `formatMessage` intrinsic returns the raw pattern | **fixed** 2026-08-12, W7-43-formatmessage-substitution.md |
| 57,60 | the supplier record must reach the `StreamHandler` | §4 | **fixed** (predicted) |
| 59 | `SimpleFormatter` must render the inferred source class/method | records carry no `sourceClassName`/`sourceMethodName` | **open**, §7 |

That is why §6.2 schedules this vector in `JDKONLY_CLASSES` and **not** in
`CORE_CLASSES`: four measured `Compatible` divergences remain, in files this
lane does not own. Promoting it to `CORE_CLASSES` is a real and worthwhile
follow-up and the list above is its work item.

**This vector is expected RED under `--jdk-only` until §6.1 lands.** That is the
gate doing its job — it is the executable form of the regression. After §6.1 it
should go green, because every surface it touches then runs as real bytecode
against the already-real static manager of §2; that prediction has not been run
and §8 says so.

---

## 6. Out-of-file patches (not applied)

### 6.1 `native-builtins/src/phases_early.rs` — the fix

Two `LogManager` registrations sit at the tail of
`register_phase54_logging_extras`, inheriting the function-wide `Intrinsic`
ambient category set at its head. They are not intrinsics. Registering them as
what they are makes the retirement able to see them, and nothing else changes.

```rust
    // --- LogManager (singleton) ---
    //
    // NOT `Intrinsic`, unlike the rest of this function, and the distinction
    // is load-bearing rather than cosmetic. These two FABRICATE a manager and
    // a logger in front of real bytecode — a `Bridge` by definition; an
    // intrinsic is the kind that cannot give an answer the bytecode would not.
    // `Intrinsic` is exempt from the `java/util/logging/` shadow retirement, so
    // while the ambient category applied here these rows SURVIVED `--jdk-only`
    // after the retirement refused every other registrar of the same triples —
    // and a refusal does not remove the registration it would have overwritten.
    // The result was an uncached fabricator holding `getLogManager()` and
    // returning a fresh, unconstructed manager on every call, whose null
    // `systemContext` is the first NPE any JUL user hits. See
    // docs/known-issues/jdk-only/W7-25-jul-getlogger-regression.md.
    let lm = "java/util/logging/LogManager";
    r.with_category(cratonvm_native_api::NativeKind::Bridge, |r| {
        r.register(
            lm,
            "getLogManager",
            "()Ljava/util/logging/LogManager;",
            |ctx, _args| {
                let obj = try_alloc_concurrent_synthetic(ctx, "java/util/logging/LogManager", 0)?;
                Ok(Some(Value::Object(Some(obj))))
            },
        );
        r.register(
            lm,
            "getLogger",
            "(Ljava/lang/String;)Ljava/util/logging/Logger;",
            |ctx, _args| {
                // Return a new Logger stub
                let logger = try_alloc_concurrent_synthetic(ctx, "java/util/logging/Logger", 2)?;
                Ok(Some(Value::Object(Some(logger))))
            },
        );
    });
```

Only the two `r.register` calls move inside the closure; their bodies are
unchanged.

**Why this is the minimal form.** `Compatible` is untouched: the rows still
register, and `logmanager.rs:5472` still overwrites them last and wins, so
`--real-jdk` dispatch is byte-for-byte what it is today. Synthetic-JDK mode is
untouched: the retirement only refuses under `--jdk-only`. Under `--jdk-only`
all three registrars of `getLogManager` are now refused, no native holds the
triple, and the real bytecode returns the already-constructed static manager of
§2.

**Verification that must accompany it**, and it is one run, not a rebuild-only
claim:

1. `--jdk-only --explain-jdk-only --dump-native-registry` — the row
   `java/util/logging/LogManager.getLogManager()…` must be **absent**, not
   merely re-kinded. Today it is present as `intrinsic`.
2. The three-call identity probe must return one object, matching `Compatible`
   and HotSpot.
3. `RJdkLogging` under `--jdk-only` must reach all 63 checks.
4. Re-take the census in both modes and diff the surviving `java/util/logging/`
   rows per triple. Two registrations move `Intrinsic` → `Bridge`, so
   `BASELINE_SYNTHETIC_STUBS` and `bridge_shadows_bytecode` both move; re-freeze
   the pair from one real run on `25/linux`, which is not this lane's platform.

### 6.2 `regression-suite/run.sh` — scheduling

Line 110. Append `RJdkLogging` to `JDKONLY_CLASSES`, and nothing else — the
list is explicit, nothing schedules by glob, and a second registration in
`CORE_CLASSES` would run the identical command twice under
`CRATONVM_ARGS="--jdk-only"`.

```diff
-JDKONLY_CLASSES="RJdkHello RJdkStrict RJdkCollections RJdkLambdas RJdkHandles RJdkProxy RJdkReflect RJdkFieldModule RJdkRecords RJdkHidden RJdkModule RJdkServices RJdkAqs RJdkPhaser RJdkExecutors RJdkForkJoin RJdkNio RJdkNet RJdkProcess RJdkSecurity RJdkJmx RJdkJni RJdkFailure RJdkStampedStamps RJdkLookupIn RJdkDefineClass RJdkX509Intercept"
+JDKONLY_CLASSES="RJdkHello RJdkStrict RJdkCollections RJdkLambdas RJdkHandles RJdkProxy RJdkReflect RJdkFieldModule RJdkRecords RJdkHidden RJdkModule RJdkServices RJdkAqs RJdkPhaser RJdkExecutors RJdkForkJoin RJdkNio RJdkNet RJdkProcess RJdkSecurity RJdkJmx RJdkJni RJdkFailure RJdkStampedStamps RJdkLookupIn RJdkDefineClass RJdkX509Intercept RJdkLogging"
```

It needs no `class_cv_args` hook: it takes no flags of its own beyond the mode
the runner already passes. Schedule it **with** §6.1 or knowingly before it —
see the RED note at the end of §5.

---

## 7. Still open, measured, not fixed

| defect | mode | owner | note |
|---|---|---|---|
| `getLogManager()` mints a fresh unconstructed manager per call | `--jdk-only` | `phases_early.rs` | §1; fix is §6.1 |
| `setUseParentHandlers(false)` does not stop the publication walk | `Compatible` | the write is `lib.rs`'s side table, the read is `logmanager.rs`'s `jul_use_parent_handlers` | the accessor agrees with itself — `getUseParentHandlers()` returns `false` correctly — while the consumer reads `config.useParentHandlers`, null on a synthetic logger. The clean fix is one visibility change on `jul_logger_use_parent_handlers_table` plus a first-choice read in `jul_use_parent_handlers`; the in-file-only workaround (dispatch `getUseParentHandlers()Z`) puts a Java invoke on every publication and NPEs on a null `config` in strict, so it was not taken |
| `info`/`warning`/`fine`/`severe`/`finest`(Supplier) evaluate a suppressed supplier | `Compatible` | `lib.rs:17076-17106` | same defect as §4, same shape of fix |
| `Formatter.formatMessage` returns the raw `{0}` pattern | `Compatible` | the `formatMessage` intrinsic | HotSpot `one=A two=B`, CratonVM `one={0} two={1}` — **FIXED both modes 2026-08-12**, W7-43-formatmessage-substitution.md |
| records carry no inferred `sourceClassName`/`sourceMethodName` | `Compatible` | `logmanager.rs` record construction | `SimpleFormatter` renders the logger name where HotSpot renders `Class method` |
| `LogManager.getLogger` demand-creates for an undemanded name | `Compatible` | `logmanager.rs` `native_get_logger` | HotSpot `null`; the JULI shims depend on the demand-creation, so this is a design change, not a patch |
| `log(LogRecord)` is not level-gated | both | `logmanager.rs` | not measured, predicted from the same missing gate as §4; HotSpot drops a record below the logger's level |

---

## 8. What is proven and what is not

**Proven by running the pre-built binary** (2026-08-11 19:41, HotSpot 25.0.3+9
control on every arm):

* the reproduction and the three-way mode split (§0);
* the registrar attribution chain and the `Intrinsic` exemption (§1), read from
  each mode's own census `registered_by`/`overwrote`, plus the per-call
  identity measurement, reproduced under `--nojit`;
* the static `LogManager.manager` field table (§2), by reflection, both modes;
* the supplier evaluation and delivery measurements (§4);
* `RJdkLogging` passing 63/63 on HotSpot, byte-identical over three runs, and
  the full 63-check divergence census in both CratonVM modes (§5).

**Not proven, and not claimed. Nothing was rebuilt.**

* The §4 fix was not run. It is reasoned from a measured defect and a measured
  helper, and the `Compatible` effect table is derived by reading
  `jul_log_parameterized`'s fallback, not by observing it.
* The §6.1 patch was not compiled or applied, and the prediction that
  `RJdkLogging` goes green under `--jdk-only` after it has not been run.
* No ratchet number here is a measurement; §6.1's baselines must be re-frozen
  from one real run on the platform they are keyed to.
* The strict corpus was not re-run. The available binary is not this branch's,
  and running `regression-suite/run.sh` against a foreign binary would
  attribute its results to source it was not built from.
