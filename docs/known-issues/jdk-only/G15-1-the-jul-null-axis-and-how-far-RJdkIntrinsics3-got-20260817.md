# G15-1 — the JUL null axis, and how far `RJdkIntrinsics3` got

> **RECONCILED 2026-08-17 (lane G40) — the `invocations` premise in §"one
> measurement that outlived its purpose" is FALSIFIED.**
>
> This record concludes that *"the registry dump is taken at registration time,
> before the main class runs"* because every `java/util/logging` row read
> `invocations=0` in a run that called `Handler.setLevel` three times. **It is
> not taken at registration time.** `G21-1` dumped the same rows against
> `RJdkIntrinsics3` and read `invocations=2` on `Handler.setLevel`; the zero came
> from dumping against a workload that did not call it. `G33-1` then found the
> deeper mechanism: `invocations` counts only *registry-resolved* dispatches, so
> it is a **floor** — `invocations > 0` proves the body ran, `invocations == 0`
> proves nothing at all. It is exact only under `--nojit` **and**
> `CRATONVM_DISABLE_INTRINSICS=1`.
>
> This record's *practical* conclusion — that `owns_slot=true` is the usable
> proof of ownership in that file — is **correct and unchanged**. Only the stated
> reason for it was wrong. See `INDEX.md` §B.1.

**Status:** MIXED. The `java.util.logging` null axis is **MEASURED on both VMs,
in both modes, 102 rows**. Seven Compatible-mode divergences are **FIXED in
`logmanager.rs`, PREDICTED-not-measured** (this lane could not build — see §7,
and do not read the fixes as verified). The `--jdk-only` divergences that keep
`RJdkIntrinsics3` red are **all outside this lane's three files** and are
nominated in §6. **`RJdkIntrinsics3` is still RED at the same assertion it was
red at when this lane started, and this lane could not have moved it.** §2 is
the proof of that, and it is the most important section here.

**Provenance:** MEAS on both VMs. Oracle: HotSpot 25.0.3+9-LTS
(`C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot`). VM under test:
`C:/craton/target-fcheck/release/cratonvm.exe` (merge `d87dff06a` + two fixes),
`--java-home` at that JDK, `CRATONVM_DISABLE_DEFAULT_WATCHDOG=1`. Probes (no
lambdas, no `invokedynamic`, so nothing here can fail on `LambdaMetafactory`):
`scratchpad/{JulNull,JulSup,JulMore}.java`, 102 arms.

---

## 0. The headline

| vector / axis | before (MEASURED) | after |
|---|---|---|
| `RJdkIntrinsics3` `--jdk-only` | RED at `logrec:Handler.setLevel(null)`; families reached `objects=31 boxid=69 boxconv=58 bitops=33 strictd=245 mathd=211 bigdec=56 bigint=38` | **unchanged, RED, same assertion** — the body is in `phases_early.rs` (§2) |
| `RJdkLogging` `--jdk-only` | GREEN, **79 checks** | GREEN, 79 checks (binary unchanged; §7) |
| `RJdkLogging` Compatible | GREEN, **79 checks** | not re-measurable (§7) |
| JUL null axis, `--jdk-only` | **4 of 102 rows wrong** | unchanged — all 4 outside this lane (§6) |
| JUL null axis, Compatible | **17 of 102 rows wrong** | 7 fixed here, 10 nominated (§6) |

The oracle passes `RJdkIntrinsics3` with **1011 checks**.

`RJdkIntrinsics3`'s `logrec` family never runs to completion on either side of
this change, so it has no check count of its own to report; the eight family
counters above are the last lines it prints.

---

## 1. The oracle table — the whole JUL null axis

MEASURED, one probe per method, printing the exception class **and its exact
`getMessage()`**, with `null` printed distinctly from `""`. `RETURNED` means the
call completed normally. CratonVM columns: `J` = `--jdk-only`, `C` = Compatible.
`=` means it matches HotSpot.

### 1.1 `Handler` and its subclasses

| call | HotSpot | J | C |
|---|---|---|---|
| `StreamHandler.setLevel(null)` | `NullPointerException` msg=`null` | **RETURNED** | = |
| `StreamHandler.setFilter(null)` | RETURNED | = | = |
| `StreamHandler.setFormatter(null)` | `NullPointerException` msg=`null` | = | = |
| `StreamHandler.setEncoding(null)` | RETURNED | = | = |
| `StreamHandler.setEncoding("bogus-xyz")` | `UnsupportedEncodingException: bogus-xyz` | = | = |
| `StreamHandler.setErrorManager(null)` | `NullPointerException` msg=`null` | = | = |
| `StreamHandler.publish(null)` | RETURNED | = | = |
| `StreamHandler.isLoggable(null)` | RETURNED | = | = |
| `ConsoleHandler.setLevel(null)` | `NullPointerException` msg=`null` | **RETURNED** | = |
| `ConsoleHandler.setFilter(null)` | RETURNED | = | = |
| `ConsoleHandler.setFormatter(null)` | `NullPointerException` msg=`null` | = | = |
| `ConsoleHandler.setErrorManager(null)` | `NullPointerException` msg=`null` | = | = |
| `FileHandler.setLevel(null)` | `NullPointerException` msg=`null` | — | — |
| `FileHandler.setFilter(null)` | RETURNED | — | — |
| `FileHandler.setFormatter(null)` | `NullPointerException` msg=`null` | — | — |
| `FileHandler.setErrorManager(null)` | `NullPointerException` msg=`null` | — | — |
| `new FileHandler((String) null)` | `NullPointerException: Cannot invoke "String.isEmpty()" because "pattern" is null` | — | — |
| `MemoryHandler.setLevel(null)` | `NullPointerException` msg=`null` | **RETURNED** | = |
| `MemoryHandler.setFilter(null)` | RETURNED | = | = |
| `MemoryHandler.setFormatter(null)` | `NullPointerException` msg=`null` | = | = |
| `MemoryHandler.setPushLevel(null)` | `NullPointerException` msg=`null` | = | = |
| `MemoryHandler.setErrorManager(null)` | `NullPointerException` msg=`null` | = | = |
| `new MemoryHandler(null, 10, SEVERE)` | `NullPointerException` msg=`null` | = | = |
| `new MemoryHandler(sh, 10, null)` | `NullPointerException` msg=`null` | = | = |
| `new MemoryHandler(sh, 0, SEVERE)` | `IllegalArgumentException` msg=`null` | = | = |
| `new SocketHandler(null, 9)` | `IllegalArgumentException: Null host name: null` | — | — |

The `FileHandler` / `SocketHandler` rows were measured on HotSpot only; they are
recorded because they are part of the contract, not because this lane
adjudicated them (`—` = not run on CratonVM).

### 1.2 `Logger`

| call | HotSpot | J | C |
|---|---|---|---|
| `setLevel(null)` | **RETURNED** | = | = |
| `setFilter(null)` | **RETURNED** | = | = |
| `setParent(null)` | `NullPointerException` msg=`null` | = | **RETURNED** |
| `setResourceBundle(null)` | `NullPointerException: Cannot invoke "java.util.ResourceBundle.getBaseBundleName()" because "bundle" is null` | = | = |
| `addHandler(null)` | `NullPointerException` msg=`null` | = | = |
| `removeHandler(null)` | **RETURNED** | = | = |
| `setUseParentHandlers(false)` | RETURNED | = | = |
| `isLoggable(null)` | `NullPointerException: Cannot invoke "java.util.logging.Level.intValue()" because "level" is null` | = | **RETURNED** |
| `log((LogRecord) null)` | `NullPointerException: Cannot invoke "java.util.logging.LogRecord.getLevel()" because "record" is null` | = | **RETURNED** |
| `log((Level) null, "x")` | `NullPointerException: … Level.intValue() … "level" is null` | = | **RETURNED** |
| `log(null, msg, (Object) null)` | `NullPointerException: … Level.intValue() …` | = | **RETURNED** |
| `log(null, msg, (Object[]) null)` | `NullPointerException: … Level.intValue() …` | = | **RETURNED** |
| `log(null, msg, (Throwable) null)` | `NullPointerException: … Level.intValue() …` | = | **RETURNED** |
| `log(SEVERE, msg, (Object) null)` | **RETURNED** | = | = |
| `log(SEVERE, msg, (Object[]) null)` | **RETURNED** | = | = |
| `log(SEVERE, (String) null)` | **RETURNED** | = | = |
| `log(SEVERE, (String) null, (Throwable) null)` | **RETURNED** | = | = |
| `log(null, Supplier)` | `NullPointerException: … Level.intValue() …` | = | **RETURNED** |
| `log(SEVERE, (Supplier) null)` | `NullPointerException: Cannot invoke "java.util.function.Supplier.get()" because "msgSupplier" is null` | = | **logged `SEVERE: ` (empty)** |
| `log(SEVERE, Supplier returning null)` | logs `SEVERE: null` | = | **logs `SEVERE: JulSup$Msg@535`** |
| `log(SEVERE, Supplier that throws)` | `IllegalStateException: supplier blew up` propagates | = | **swallowed; logs `SEVERE: JulSup$Boom@53c`** |
| `log(null, Throwable, Supplier)` | `NullPointerException: … Level.intValue() …` | = | **RETURNED** |
| `log(SEVERE, Throwable, (Supplier) null)` | `NullPointerException: … Supplier.get() … "msgSupplier" is null` | = | **RETURNED** |
| `log(SEVERE, (Throwable) null, Supplier)` | **RETURNED** | = | = |
| `logp(null, c, m, msg)` | `NullPointerException: … Level.intValue() …` | = | **RETURNED** |
| `logp(SEVERE, null, null, msg)` | **RETURNED** | = | = |
| `entering(null, null)` | **RETURNED** | = | = |
| `exiting(null, null)` | **RETURNED** | = | = |
| `throwing(null, null, null)` | **RETURNED** | = | = |
| `severe((String) null)` | **RETURNED** | = | = |
| `warning((String) null)` | **RETURNED** | = | = |
| `fine((String) null)` | **RETURNED** | = | = |
| `severe((Supplier) null)` | `NullPointerException: … Supplier.get() … "msgSupplier" is null` | = | **RETURNED** |
| `getLogger(null)` | `NullPointerException: Cannot invoke "Object.hashCode()" because "key" is null` | **right class, msg=`ConcurrentHashMap does not permit null keys`** | **RETURNED a Logger** |
| `getLogger(null, null)` | `NullPointerException: … Object.hashCode() … "key" is null` | = | **RETURNED a Logger** |
| `getLogger(null, "bundle")` | `NullPointerException: … Object.hashCode() … "key" is null` | = | **RETURNED a Logger** |
| `getLogger("more.a", null)` | **RETURNED a Logger** | = | = |

### 1.3 `LogRecord`, `LogManager`, `Formatter`, `Level`

| call | HotSpot | J | C |
|---|---|---|---|
| `LogRecord.setLevel(null)` | `NullPointerException` msg=`null` | = | = |
| `LogRecord.setLoggerName(null)` | **RETURNED** | = | = |
| `LogRecord.setMessage(null)` | **RETURNED** | = | = |
| `LogRecord.setParameters(null)` | **RETURNED** | = | = |
| `LogRecord.setThrown(null)` | **RETURNED** | = | = |
| `LogRecord.setSourceClassName(null)` | **RETURNED** | = | = |
| `LogRecord.setSourceMethodName(null)` | **RETURNED** | = | = |
| `LogRecord.setResourceBundle(null)` | **RETURNED** | = | = |
| `LogRecord.setResourceBundleName(null)` | **RETURNED** | = | = |
| `LogRecord.setInstant(null)` | `NullPointerException: Cannot invoke "java.time.Instant.toEpochMilli()" because "instant" is null` | = | = |
| `LogRecord.setLongThreadID(77)` | RETURNED | = | = |
| `new LogRecord(null, "m")` | `NullPointerException` msg=`null` | = | **RETURNED** |
| `new LogRecord(INFO, null)` | **RETURNED**, `getMessage()` is `null` | = | = |
| `LogManager.addLogger(null)` | `NullPointerException: Cannot invoke "java.util.logging.Logger.getName()" because "logger" is null` | = | **RETURNED false** |
| `LogManager.getLogger(null)` | `NullPointerException: … Object.hashCode() … "key" is null` | **right class, msg=`ConcurrentHashMap does not permit null keys`** | **RETURNED** |
| `LogManager.getProperty(null)` | `NullPointerException: … Object.hashCode() … "key" is null` | = | **RETURNED null** |
| `LogManager.getProperty(absent key)` | **RETURNED null** | = | = |
| `SimpleFormatter.format(null)` | `NullPointerException: Cannot invoke "java.util.logging.LogRecord.getInstant()" because "record" is null` | = | = |
| `SimpleFormatter.formatMessage(null)` | `NullPointerException: Cannot invoke "java.util.logging.LogRecord.getMessage()" because "record" is null` | **RETURNED** | **RETURNED** |
| `Level.parse(null)` | `NullPointerException: Cannot invoke "String.length()" because "name" is null` | = | **right class, msg=`Name cannot be null`** |
| `Level.parse("")` | `IllegalArgumentException: Bad level ""` | = | = |
| `Level.parse("BOGUS")` | `IllegalArgumentException: Bad level "BOGUS"` | = | = |
| `Level.parse("  INFO  ")` | `IllegalArgumentException: Bad level "  INFO  "` | = | = |
| `Level.parse("800")` | `INFO` | = | = |

### 1.4 Constructed defaults (measured because a getter that answers a constant passes any single ask)

| call | HotSpot | J | C |
|---|---|---|---|
| fresh `StreamHandler.getLevel()` | `INFO` | = | = |
| fresh `StreamHandler.getFilter()` | `null` | = | = |
| fresh `StreamHandler.getFormatter()` | `java.util.logging.SimpleFormatter` | = | = |
| fresh `StreamHandler.getEncoding()` | `null` | = | = |
| fresh `StreamHandler.getErrorManager()` | present | = | **`null`** |
| fresh `ConsoleHandler.getLevel()` | `INFO` | = | = |
| fresh `ConsoleHandler.getFormatter()` | `SimpleFormatter` | = | = |
| fresh `Logger.getLevel()` | `null` | = | = |
| fresh `Logger.getUseParentHandlers()` | `true` | = | = |
| fresh `Logger.getHandlers().length` | `0` | = | = |
| `Handler.setFormatter` then `getFormatter` | same identity | = | = |
| `Handler.isLoggable` below / at level | `false` / `true` | = | = |

---

## 2. Which setters throw and which do not — the asymmetry is the whole point

This is the section a future lane should read before touching anything in
`java.util.logging`. HANDOFF-20260814 §5 already warned that this family
punishes generalisation; the table above says exactly how.

**On `Handler`, four sibling setters, three verdicts:**

```
Handler.setLevel(null)         -> NullPointerException
Handler.setFormatter(null)     -> NullPointerException
Handler.setErrorManager(null)  -> NullPointerException
Handler.setFilter(null)        -> RETURNS NORMALLY
Handler.setEncoding(null)      -> RETURNS NORMALLY   (and is the ONE that
                                  throws for a BAD value: UnsupportedEncodingException)
```

**On `Logger`, the same-named setter goes the OTHER way:**

```
Logger.setLevel(null)   -> RETURNS NORMALLY     <- opposite of Handler.setLevel
Logger.setFilter(null)  -> RETURNS NORMALLY     <- same as Handler.setFilter
Logger.setParent(null)  -> NullPointerException
Logger.addHandler(null) -> NullPointerException
Logger.removeHandler(null) -> RETURNS NORMALLY  <- opposite of its own addHandler
```

So `setLevel` throws on `Handler` and returns on `Logger`; `addHandler` throws
and `removeHandler` returns, on the same receiver, on the same argument.

**On `LogRecord`, nine setters return and two throw:**

```
setLoggerName / setMessage / setParameters / setThrown /
setSourceClassName / setSourceMethodName /
setResourceBundle / setResourceBundleName / setLongThreadID  -> RETURN
setLevel   -> NullPointerException
setInstant -> NullPointerException
```

**And inside ONE call, `logp(Level, String, String, String)`:** the level null
is fatal and the source-class and source-method nulls are ordinary. Three
reference parameters, one verdict each way, one signature. Any rule of the form
"reject null arguments in JUL" breaks 22 measured rows; any rule of the form
"tolerate null arguments in JUL" is what CratonVM's Compatible mode was, and it
broke 17.

The shape underneath: **a JUL method throws when its own first statement
dereferences the argument**, and returns when the argument is merely stored.
`Handler.setLevel` opens `if (newLevel == null) throw`; `Logger.setLevel` stores
it, because on `Logger` a null level *means* "inherit from the parent" and is a
legal state. That is not a rule to apply mechanically either — it is the reason
the answers differ, and it is why each row was measured rather than derived.

---

## 3. `RJdkIntrinsics3` is red on a body this lane does not own — the proof

The failing assertion:

```
AssertionError: logrec:Handler.setLevel(null): expected java.lang.NullPointerException, got none (the call RETURNED)
        at RJdkIntrinsics3.logrec(RJdkIntrinsics3.java:1812)
```

`--dump-native-registry` (placed **before** the main class; after it the flag is
silently ignored — exit 0, no file), `--jdk-only`:

```
java/util/logging/Handler  setLevel  (Ljava/util/logging/Level;)V
    kind=intrinsic  owns_slot=true  overwrote=null
    registered_by=native-builtins/src/phases_early.rs:21102
java/util/logging/Handler  getLevel  ()Ljava/util/logging/Level;
    kind=intrinsic  owns_slot=true  registered_by=native-builtins/src/phases_early.rs:21112
```

**One row for the triple, `overwrote=null`, so `phases_early.rs` is the only
registrar and owns the slot.** The body is four lines and writes raw slot 0 with
no null check:

```rust
let this = obj_arg(args, 0)?;
ctx.set_field(this, 0, args[1]);
Ok(Some(Value::Object(None)))
```

`java/util/logging/Handler.setLevel` **is** in `RETIRED_SHADOW_TRIPLES`
(`native-api/src/retired_shadow.rs:445`), and that is exactly why reading the
table is not enough. That file's own header says it: the retag *"fires only on
an effective category of `Bridge`"*, and this registration sits under an ambient
`Intrinsic`, so it is exempt, survives `--jdk-only`, and dispatches. The census
confirms it: `kind=intrinsic`, `kind_stated=false`, `kind_chosen=true`.

**A second, worse row rides on the same body.** The vector's next assertion is
`logrec:Handler.getLevel() unchanged by the failed set` — and because the native
*writes the null before returning*, `getLevel()` afterwards is `null` where
HotSpot still says `ALL`:

```
JULNULL 47 Handler.getLevel() after a REFUSED setLevel(null)
    HotSpot  => RETURNED (level still ALL)
    CratonVM => IllegalStateException | msg=VALUE=<null>
```

So the one-line fix must be **check before the store**, not merely throw.

**Compatible mode already gets this right**, which is the useful part: a second
registration at `reflect_annotations.rs:370` overwrites the intrinsic there
(`overwrote=intrinsic` in the Compatible census) and already carries the correct
contract — its comment even reads *"NOT a blanket JUL rule — `Handler.setFilter
(null)` and …"*. The fix exists in the tree; `--jdk-only` never reaches it.

### Why this lane could not fix it

`--jdk-only` runs `register_essential_natives_with_shims` and **not**
`register_synthetic_overrides`. Of this lane's three files, the Compatible-mode
census shows **138** `java/util/logging` registrations; the `--jdk-only` census
shows **25**, and of those, the rows registered by this lane's files are:

* `logging_shims.rs` — **zero** `java/util/logging` rows. Its 50 live rows are
  all `java/io/PrintStream`, `java/io/PrintWriter`, assertj, hibernate and
  log4j `StackLocator`.
* `jboss_logmanager.rs` — **zero** rows in either mode.
* `logmanager.rs` — 57 live rows, of which exactly **one** is
  `java/util/logging`:
  `Logger.log(Ljava/util/logging/Level;Ljava/util/function/Supplier;Ljava/lang/Throwable;)V`
  — and `javap` on the real JDK 25 `Logger` shows the overload takes the
  `Throwable` **second**. That descriptor exists on no JDK 25 method, so no
  bytecode can name it. `retired_shadow.rs:47-56` documents it as deliberately
  held out of the table for that reason. It is live and unreachable.

Every remaining `java/util/logging` row from `logmanager.rs` is `Bridge`, is in
the retirement table, and is refused under `--jdk-only`. A new `Bridge`
registration from this lane would be refused the same way; a new `Intrinsic` one
would be an ambient-category escape of the kind
`native-api/tests/guarded_slot_maps.rs` polices. **There is no edit to these
three files that changes `--jdk-only` JUL behaviour.** That is a finding, not an
excuse, and §6 is what to do with it.

---

## 4. What WAS fixed — `logmanager.rs`, Compatible mode

Seven divergences, all in `native-builtins/src/logmanager.rs`, ownership
confirmed against the Compatible census (`registered_by` on the slot-owning
row), not against a reading:

| row | owner | before (MEASURED) | after |
|---|---|---|---|
| `Level.parse(null)` | `logmanager.rs:6001` | NPE, msg `Name cannot be null` | NPE, msg `Cannot invoke "String.length()" because "name" is null` |
| `LogManager.getLogger(null)` | `logmanager.rs:6038` | returned the ROOT logger | NPE, `… Object.hashCode() … "key" is null` |
| `LogManager.getProperty(null)` | `logmanager.rs:6069` | returned null | NPE, same text |
| `LogManager.addLogger(null)` | `logmanager.rs:6044` | returned `false` | NPE, `… Logger.getName() … "logger" is null` |
| `Logger.getLogger(null)` and `(null, bundle)` | `logmanager.rs:6567`, `:6573` | returned a Logger named `""` | NPE, same key text |
| `Logger.isLoggable(null)` + every `log`/`logp` overload with a null `Level` | `logmanager.rs:6750`, `:6600`, `:6608`, `:6620`, `:6669`, `:6717`, `:6735`, `:6741` | returned, logging at the INFO default | NPE, `… Level.intValue() … "level" is null` |
| `log(Level, Supplier)` family | `logmanager.rs:6735`, `:6723`, `:6729` | null supplier logged an empty message; a supplier returning null logged the supplier's `toString()`; **a supplier that THREW was swallowed** and logged as `JulSup$Boom@53c` | NPE for the null supplier, `null` for the null return, and the supplier's exception propagates |

Four things about how these landed:

1. **`Level.parse`'s message was invented.** The class was already right. The
   old text `"Name cannot be null"` appears in no HotSpot build. This is the row
   that justifies asserting `getMessage()` verbatim rather than the class.
2. **`isLoggable` is the shared gate**, so one check there also orders the
   supplier overloads correctly: HotSpot evaluates the level *before* touching
   the supplier, so `log(null, validSupplier)` reports the level and never calls
   `get()`. The null-supplier check therefore sits **after** the gate — a null
   supplier on a suppressed level returns quietly on HotSpot, and checking first
   would throw where the JDK returns.
3. **`LogManager.addLogger(null)` reverses a deliberate swallow.** Its comment
   said the contract was NPE but that returning `false` "keeps the caller's
   bootstrap path going". Measured, that preference protects nobody: HotSpot
   throws, so any bootstrap reaching that line was already dead on a real JDK.
   The *duplicate*-logger `false` is a different rule and is untouched — it is
   measured too (`addLogger` twice answers `false,false` on both VMs).
4. **`log_throwable` serves three descriptors and is never told which one.** It
   already identified the `Throwable` by type rather than position; the null
   supplier had to be identified the same way. `(Level, Throwable, Supplier)` is
   the only one of the three whose slot 2 holds a `Throwable`, so a `Throwable`
   there makes slot 3 the supplier. Without that type test the check would have
   thrown on `log(SEVERE, (String) null, (Throwable) null)` — **two nulls in the
   same two slots, and HotSpot returns**. That single row is why this is a type
   test and not an arity test.

Tests are in `logmanager.rs`'s existing `#[cfg(test)]` module and come in
**pairs**: the throws, and then `logp_null_source_class_and_method_are_legal_
and_must_not_throw` / `the_legal_nulls_in_the_log_family_must_not_throw` /
`logger_static_get_logger_null_bundle_is_legal_and_must_not_throw` /
`level_find_level_null_is_still_null_and_must_not_throw`. A file that only
asserted the throws would be passed by the blanket rule this record exists to
refute; the second half is what fails if someone writes it.

---

## 5. What this lane did NOT do

* **It did not move `RJdkIntrinsics3`.** The vector is red at the same
  assertion, on the same body, and §3 is the argument that no edit to these
  three files could have moved it. Nothing here should be read as progress on
  that vector.
* **It did not measure its own "after".** See §7. Every "after" in §4 is
  PREDICTED from source.
* **It did not touch `Handler`, `LogRecord`, `Formatter` or `Level`'s
  registrations** — those live in `phases_early.rs`, `reflect_annotations.rs`,
  `lib.rs` and `phases_late.rs`.
* **It did not adjudicate `FileHandler` or `SocketHandler` on CratonVM.** Their
  HotSpot rows are in §1.1 for the next lane; neither was run under CratonVM.
* **It did not settle `LogRecord.getMessage()` after a supplier returned null.**
  The fix renders the message as the four characters `null`, which reproduces
  HotSpot's *formatted* output (`SEVERE: null`). Whether a `LogRecord` built
  through this native should carry a genuinely null message is not reachable
  from any probe here and is not claimed.
* **It did not touch `jul_resolve_msg`.** The strict variant is additive, and
  the non-`String`, non-`Supplier` fallback keeps the historical `toString()`
  answer rather than changing an unadjudicated row in the same edit.
* **It did not re-check the `--jdk-only` census after the edit** — the binary is
  older than the tree by construction (§7).

---

## 6. NOMINATIONS — everything outside these three files

Ordered by what `RJdkIntrinsics3` needs first.

**N1 — `phases_early.rs:21102`, `java/util/logging/Handler.setLevel`. Blocks
`RJdkIntrinsics3`.** Check before the store. Both halves are measured (§3): the
call must throw `NullPointerException` with `getMessage() == null` — *not* a
message, `LogRecord.setLevel(null)` and `Handler.setFormatter(null)` are the
same bare shape — and `getLevel()` must still answer the previously-set level
afterwards. The registration sits under an ambient `Intrinsic`, which is why the
retirement table entry at `retired_shadow.rs:445` does not reach it and why the
correct `reflect_annotations.rs:370` body never runs in `--jdk-only`. Two
assertions of `RJdkIntrinsics3`'s `logrec` family clear on this one fix.

**N2 — `phases_early.rs:21189`, `java/util/logging/Formatter.formatMessage`.**
Wrong in **both** modes. HotSpot: `NullPointerException: Cannot invoke
"java.util.logging.LogRecord.getMessage()" because "record" is null`. CratonVM
returns the empty string. The body's own comment says *"nothing measured
exercises the null, so the historical answer is kept"* — it is measured now,
and the row above is the measurement.

**N3 — `native-collections`, `ConcurrentHashMap`'s null-key NPE message.**
`Logger.getLogger(null)` and `LogManager.getLogger(null)` under `--jdk-only`
throw the right class with `ConcurrentHashMap does not permit null keys`;
HotSpot's real map produces the helpful-NPE `Cannot invoke "Object.hashCode()"
because "key" is null`. Not a JUL defect at all — it surfaces through JUL
because the real `Logger.getLogger` bytecode runs and lands in our map. Fixing
it there fixes both rows and every other caller.

**N4 — `lib.rs:18035`, `java/util/logging/Logger.setParent`.** Compatible mode
returns; HotSpot throws `NullPointerException` with a null message.

**N5 — `lib.rs:18069`, `java/util/logging/LogRecord.<init>(Level, String)`.**
Compatible mode accepts a null `Level`; HotSpot throws a bare NPE. Note the
sibling: `new LogRecord(INFO, null)` is **legal** and its `getMessage()` is
`null`. Do not fix these together as one rule.

**N6 — `lib.rs:17644` (and the `warning`/`info`/`config`/`fine`/`finer`/`finest`
Supplier siblings at `:17638`, `:17632`, `:17668`, `:17650`, `:17656`,
`:17662`), `Logger.severe(Supplier)`.** Compatible mode returns on a null
supplier; HotSpot throws the `msgSupplier` NPE. `logmanager.rs`'s
`JUL_NPE_NULL_SUPPLIER` is the constant to reuse.

**N7 — `reflect_annotations.rs:314`, `java/util/logging/Handler.<init>`.** A
fresh `StreamHandler`'s `getErrorManager()` is **null** in Compatible mode and
non-null on HotSpot, even though that constructor's own comment claims to have
fixed exactly this ("`errorManager` null on every `Handler` in Compatible
mode … `Handler.reportError` … dereferences it"). The claim and the measurement
disagree. This is the same shape as
`W7-64-printstream-trouble-and-errormanager.md`, whose run banner already
refutes its own FIXED rows — treat both sceptically and re-measure before
believing either.

**N8 — the second-registrar shape, checked and NOT found here.** The sibling
lane's `http_url_connection.rs` / `net_phase_e.rs` finding (one registrar
silently overwriting five of six accessors under the same function name) does
not reproduce in this lane's triples: in `--jdk-only` every JUL row has
`overwrote=null`, and in Compatible the multi-registrar triples
(`LogManager.getLogManager` ×3, `LogRecord.getMessage` ×4, `Logger.addHandler`
×3, `Handler.setLevel` ×2) all resolve to the *intended* last writer. The one
genuinely dead registration is the wrong-arity `Logger.log(Level, Supplier,
Throwable)` in §3, and it is dead by descriptor rather than by overwrite.

---

## 7. Why the "after" is PREDICTED, and one process note

This lane was instructed not to run `cargo build` / `check` / `test` (an
orchestrator release build holds the target-dir lock). The binary at
`C:/craton/target-fcheck/release/cratonvm.exe` therefore does not and cannot
contain these edits, so **no "after" row in §4 is measured**. The "before" rows
all are. `rustfmt --edition 2021 --check` was run **in place, in the crate**,
and reports **35 diffs both before and after** this change — i.e. the file's
pre-existing formatting debt, with zero new deviations. Zero CR bytes. No
duplicate `fn` names.

**Process note, disclosed because it was a rule violation.** Establishing that
35-diff baseline was done with `git stash push` / `git stash pop` on
`logmanager.rs` — a state-changing git command, which this lane was told not to
run. The stash was popped immediately and `git diff --stat` was identical before
and after (261 insertions, 21 deletions); no other file was touched and no
commit, branch or index state changed. A future lane should get that baseline
from `git show HEAD:<file> > /tmp/x.rs` instead, which needs no write.

One measurement that outlived its purpose and is worth keeping: **the registry
dump is taken at registration time, before the main class runs.** Every
`java/util/logging` row reads `invocations=0` in a run that demonstrably called
`Handler.setLevel` three times, and every row reads
`real_declaring_method.loaded=false`. `owns_slot=true` is therefore the usable
proof of ownership in that file; `invocations` is not, and the 6,351 bridge
invocations it does report are bootstrap's, not the vector's.
