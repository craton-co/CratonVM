# W7-35 — the JUL residuals: two of the four were mine, one was already fixed, and the two that keep the vector red are the same bug as W7-25 §1

> **MEASURED CLOSED 2026-08-12 (second pass, same day). The vector is no longer
> the suite's only red — it is green in both modes.**
>
> §6 ends "Nothing was rebuilt", and §5's prediction — that the patch takes
> `--jdk-only` to 63/63 — had never been run. Run now, on a default-feature
> release binary dated 2026-08-12 15:27, HotSpot 25.0.3+9 as oracle:
>
> | arm | §0 said | measured today |
> |---|---|---|
> | HotSpot 25.0.3+9 | 63 reachable, 0 failures | `PASS RJdkLogging (79 checks)` |
> | CratonVM `--real-jdk` | 3 failures (#39, #54, #59) | `PASS RJdkLogging (79 checks)` |
> | CratonVM `--jdk-only` | 2 failures (#54, #59) | `PASS RJdkLogging (79 checks)` |
>
> All twelve `CK` lines byte-identical across the three arms. Specifically:
>
> * **#39** (`useParentHandlers=false` must stop the walk) — §1's fix is landed
>   and **verified**; §6 listed it as reasoned-not-run.
> * **#54** (`Formatter.formatMessage` must substitute) — closed by W7-43 giving
>   the row a real body, not by §5's category patch. Confirmed in the tree:
>   `native-builtins/src/phases_early.rs:20962` registers `formatMessage` inside
>   a `with_category(Bridge)` block whose comment records that the `Bridge` tag
>   alone did **not** retire it, and the body is now `jul_formatter_format_message`.
>   `CK … formatted='one=A two=B'` in all three arms.
> * **#59** — closed in **both** halves. The strict half went through
>   `RETIRED_SHADOW_TRIPLES`, as W7-56 said and §5's `with_category` patch did
>   not: `native-api/src/retired_shadow.rs:371`–`:374` carries all four source-pair
>   triples, with a comment stating they retire as a SET. `LogRecord.<init>` is
>   at `:362`. Measured `CK … sourcePair=A_CLASS/a_method halfSetMethod=null bare=LATE`,
>   identical on HotSpot.
> * **§2.1's double-inference fix** (the second `capture_stack_trace` on every
>   published record) is in the tree and its observable — `publishedSource` — is
>   green in both modes.
>
> **What is NOT closed, and it is a `Compatible`-only residual this vector cannot
> see.** `--real-jdk` prints two console lines HotSpot prints nowhere:
> `INFO [rjdklogging.handlers] after-removal` and
> `INFO [rjdklogging.inherit.child] not-up-to-parent`. Counted: HotSpot 0,
> `--jdk-only` 0, `--real-jdk` 2. Both are records that HotSpot routes to no
> handler; `jul_log_parameterized`'s console fallback publishes them anyway.
> §4's anti-silence property is what makes this invisible — every check asserts
> on a captured record or a byte count, never on stdout, so a spurious *extra*
> console line is exactly the failure shape the vector was not built to catch.
> The `not-up-to-parent` case is the same logger §1 fixed: the parent's handler
> correctly no longer receives, and the record then falls to the console instead
> of being dropped — so §1's fix is what **exposed** this half, and the two must
> be read together. Shared with W7-25 as NOMINATION 5; the fix is a
> discrimination between "no handler configured" and "handlers configured, none
> took it", plus a stdout assertion in `RJdkLogging` to gate it.
>
> **§5's remaining work item stands, narrowed by measurement.** The "24 rows
> deliberately NOT in the minimal set" are still ambient `Intrinsic` in
> `register_phase54_logging_extras`. None of them is failing a check today —
> which §5 already said is an argument for measuring them, not for leaving them
> mislabelled. The general form of that hazard was found live elsewhere today:
> `java/lang/Math.min(DD)D` is an ambient `Intrinsic` giving an answer the
> bytecode would not, invisible to both the retirement and the census
> (W7-2's 2026-08-12 block).

> **2026-08-12 — both `--jdk-only` survivors now have a landed cause, and §5's
> patch is superseded.** #54 is FIXED by giving `Formatter.formatMessage` a
> correct body (W7-43-formatmessage-substitution.md — see §5's own CORRECTION
> block). #59's strict half is FIXED in three parts in the tree by
> W7-56-infercaller-strict.md, whose cause is neither of the two this record
> named: the accessors' retirement was necessary and NOT sufficient, because a
> shadow **constructor** dropped `needToInferCaller`. Read W7-56's "Landed state"
> section for the registrar census, and do **not** apply §5's `with_category`
> patch — a category makes a row eligible for retirement, it does not retire it.
> One thing found in this record's own files while closing that: §2's Compatible
> fix had a second, weaker inference point overriding it — §2.1.

**Status:** `regression-suite/src/RJdkLogging.java` is scheduled in
`JDKONLY_CLASSES` and is the suite's only red. The full divergence census is
below — **`--jdk-only` 2 failures, `--real-jdk` 3, HotSpot 0** — measured on the
pre-built binary before any change here. Two `Compatible` defects **fixed** in
this lane's files. One of W7-25's four open rows was **already closed** and is
retracted. **Neither remaining `--jdk-only` failure is reachable from this
lane's files**, and both are one out-of-file patch, §5.

Binary for every measurement: `target/release/cratonvm.exe` as found in
`C:/craton/CratonVM`, against `Eclipse Adoptium jdk-25.0.3.9-hotspot`, with
HotSpot 25.0.3+9 as the oracle on every arm. **Nothing was rebuilt**; §6
separates what was run from what is reasoned.

---

## 0. The census, all 63 checks, three arms

W7-25 §5's soft-failure discipline, re-run: a copy of the vector with `check`
collecting instead of throwing, and each of the nine sections wrapped so one
death does not hide the sections after it. The vector itself was not touched.

| arm | reachable | failures |
|---|---|---|
| HotSpot 25.0.3+9 | 63 | **0** |
| CratonVM `--real-jdk` | 63 | **3** — #39, #54, #59 |
| CratonVM `--jdk-only` | 63 | **2** — #54, #59 |

```text
--real-jdk  XFAIL#39 useParentHandlers=false must stop the walk; got [INFO:up-to-parent, INFO:not-up-to-parent]
--real-jdk  XFAIL#54 Formatter.formatMessage must substitute; got one={0} two={1}
--real-jdk  XFAIL#59 SimpleFormatter must render the inferred source class and method; got [… rjdklogging.stream
--jdk-only  XFAIL#54 Formatter.formatMessage must substitute; got one={0} two={1}
--jdk-only  XFAIL#59 SimpleFormatter must render the inferred source class and method; got [… rjdklogging.stream
```

Read against W7-25 §5's table — `--real-jdk` 9 failures, `--jdk-only` 3 checks
reachable and 10 failures — this is what the two lanes since have bought. All
63 checks are now REACHABLE in strict mode: the W7-25 §6.1 `LogManager`
category fix landed, the `getSystemContext` NPE is gone, and eight of nine
sections that used to die on it now run.

### What the hard vector reports, and where it stops

`--jdk-only`, in file order, stopping at the first `AssertionError`:

```text
CK RJdkLogging singleton=true registryRoundTrip=true loggerNames=3
CK RJdkLogging parentHops=1 rootName='' global=global
CK RJdkLogging levelFiltering=ok records=0
CK RJdkLogging handlerChain=add,report,receive,remove records=1
CK RJdkLogging parentDelivery=[INFO:up-to-parent]
CK RJdkLogging supplier=evaluated:1 thrown=IllegalStateException
  AssertionError: Formatter.formatMessage must substitute; got one={0} two={1}
  at RJdkLogging.recordPayloads(RJdkLogging.java:411)
```

Every one of those six `CK` lines is **byte-identical to HotSpot's**, including
`supplier=evaluated:1`. That line is worth naming because it reads like a
defect and is not one: `calls[0]` is 1 because the admitted `INFO` supplier ran
exactly once and the `FINEST` one was correctly not evaluated. HotSpot prints
the same six lines. The transcript's evidence of a supplier bug is the absence
of a supplier bug — see §3.

`recordPayloads` (lines 386-414) asserts six things, in order: `log(LogRecord)`
delivers; it delivers the **same object**, not a copy of its text; the
parameterized overload delivers; the record keeps the **RAW** `one={0} two={1}`
pattern in `getMessage()`; it carries **both** parameters; those parameters are
`"A"` and `"B"`. CratonVM answers all six correctly **in both modes**. The
seventh is `new SimpleFormatter().formatMessage(r)`, and that is the one that
fails: HotSpot `one=A two=B`, CratonVM `one={0} two={1}` in both modes. So the
record is built right and only the substitution is missing — which is the shape
that points at the Formatter rather than the logging path, and §5 is where it
leads.

---

## 1. Fixed here: the `useParentHandlers` write and read never met (#39)

`Compatible` only. Measured before the change, logger at `psup.p` with a
Handler, child `psup.p.kid`:

| | `getUseParentHandlers()` | parent's Handler received |
|---|---|---|
| HotSpot, after `setUseParentHandlers(false)` | `false` | nothing |
| CratonVM `--jdk-only` | `false` | nothing |
| CratonVM `--real-jdk` | `false` | **`[INFO:not-up-to-parent]`** |

`Logger.setUseParentHandlers(Z)V` records the flag in
`jul_logger_use_parent_handlers_table` (`lib.rs`). The publication walk,
`logmanager::jul_use_parent_handlers`, read `config.useParentHandlers` — and
`config` is null on every logger this bridge mints. **The write and the read
were in different modules and never met.** The flag was accepted, stored, and
read back correctly by `getUseParentHandlers()` — an accessor agreeing with
itself — and then ignored by the only consumer that matters.

W7-25 §7 named the fix and this lane took it: make the table `pub(crate)` and
read it as the FIRST choice in `jul_use_parent_handlers`, falling through to
the old `config` read when it holds nothing. W7-25 also named the shape it
rejected — dispatching `getUseParentHandlers()Z` from the walk, which puts a
Java invoke on every publication and NPEs on a null `config` in strict — and
that judgement stands; this is the cheap version of the same answer.

**An absent entry is not `false`.** It means the setter was never called, so it
must fall through to the JDK default (`true`) rather than let a logger nobody
configured silence its ancestors' handlers. That is the difference between this
and a one-line `unwrap_or(false)`, and it is the whole reason the fallback is
kept rather than deleted.

`--jdk-only` is untouched: the census shows **no native holds either triple**
there (`java/util/logging/Logger.setUseParentHandlers` is absent from the strict
registry; only the `org/jboss/logmanager/Logger` rows survive), real bytecode
runs, and the walk already stopped. #39 does not appear in the strict column
above, and did not before this change either.

## 2. Fixed here: records carried no source class/method (#59, `Compatible` half)

`SimpleFormatter`'s default pattern renders the record's
`sourceClassName sourceMethodName` pair as `%2$s`, and **falls back to the
logger NAME when the pair is null**. So a record that arrives without it does
not render `null null` — it renders something plausible, which is why this sat
unnoticed. Measured, one `warning` through a `StreamHandler`:

| arm | rendered |
|---|---|
| HotSpot | `… PSrc main` |
| CratonVM `--real-jdk` | `… psrc.y` |
| CratonVM `--jdk-only` | `… psrc.y` |

`jul_log_parameterized` passed `None, None` for the pair into
`publish_to_jul_handlers_full`, which has carried the two parameters all along
for `logp`'s benefit. This lane fills them in when the caller supplied none.

**`LogRecord$CallerFinder` is adapted, not transcribed, and that is the point.**
Real `CallerFinder` sets `lookingForLogger = true` and returns nothing until it
has SEEN a `java.util.logging.Logger` frame — correct on HotSpot, where the
record is built inside `Logger.doLog` with those frames live. In `Compatible`
this native IS the `Logger` frame and no Java frame is pushed for it. Measured,
a `StackWalker` taken from inside a `Handler.publish`:

| arm | frames |
|---|---|
| HotSpot | `[Cap.publish, Logger.log, Logger.doLog, Logger.log, Logger.warning, PWalk.main]` |
| CratonVM `--real-jdk` | `[Cap.publish, PWalk.main]` |
| CratonVM `--jdk-only` | `[Cap.publish, Logger.log, Logger.doLog, Logger.log, Logger.warning, PWalk.main]` |

A literal transcription would hunt a marker that is never present in the mode
it was written for and answer `None` on every call — a helper that cannot fire,
landed as a fix, which is the pattern this campaign keeps cataloguing. The rule
taken instead: **the innermost Java frame, skipping any `Logger` frames that
are present.** The skip is not dead code — the mixed paths do put real `Logger`
bytecode on the stack, and without it a record would name
`java.util.logging.Logger` as its own caller. The two skipped names are exactly
the two `isLoggerImplFrame` admits and are deliberately not widened: a name
this predicate wrongly skips silently attributes the record to its caller's
caller, and nothing downstream would report it.

**Cost, stated because JUL is hot on Tomcat and Spring Boot.** The inference is
placed AFTER handler resolution, inside the branch that has already committed
to building a record for a real handler. A handler-less logger — the
pre-`readConfiguration` state both those suites boot through, which takes the
console-fallback path — pays no stack capture at all. HotSpot defers the same
cost to whoever first calls `getSourceClassName()`; this is the nearest
equivalent placement reachable from a native.

**This does not fix #59 under `--jdk-only`**, where the record is real and the
frames are already right. That half is §5.

### 2.1 The Compatible half had TWO inference points, and the weaker one won

Found and fixed 2026-08-12 while closing the strict half. `stamp_inferred_caller`
is called from `publish_to_jul_handlers_full` and stamps the pair; the block
§2 landed then ran **unconditionally afterwards** and stamped it again from
`infer_jul_caller_source`. Two consequences, both live in the tree until now:

* a **second `capture_stack_trace` on every published record** — precisely the
  cost §2 argued about, paid twice on the handler path;
* the second answer overwrote the first, and it is the **weaker** of the two
  predicates.

Which is weaker is adjudicated against the JDK 25 source rather than by
preference, because §2's own text argues for the narrow one. `LogRecord$CallerFinder.test`
has **two stages**: a latch (`isLoggerImplFrame`, exactly
`java.util.logging.Logger` and `sun.util.logging.PlatformLogger*`) that skips
until the logger is SEEN, and then a filter,
`jdk.internal.logger.SurrogateLogger.isFilteredFrame` →
`SimpleConsoleLogger.Formatting.isFilteredFrame`, which skips everything
implementing `System.Logger` plus the prefixes `java.util.logging.`,
`sun.util.logging.`, `jdk.internal.logger.`, `java.lang.invoke.MethodHandle`
and `java.security.AccessController`. So §2's "the two class names are exactly
the two `isLoggerImplFrame` admits" is right about the LATCH and wrong to use
that set as the SKIP set: those two names are the marker to look *for*, and the
frames to skip are the wider filter's. `infer_jul_caller_source` uses the latch
names as its skip set, so it can name `java.util.logging.Handler`, a
`java.lang.reflect` frame or the record's own class as the caller;
`stamp_inferred_caller`'s wider set is the analogue of the filter stage.

Fixed by making the second block a **fallback** instead of an override — it now
also requires that `sourceClassName` is not already a reference — rather than by
deleting it: the wider set can in principle reject every frame (a log driven
entirely from `java.util.logging` code), and there a narrow answer beats a null
pair, which `SimpleFormatter` renders as the logger name. On the synthetic
`LogRecord` layout both writes no-op (no field names to resolve) and the new
guard reads `Object(None)` for the absent field, so that layout is unchanged.
Unbuilt; the observable is #59's `Compatible` rendering, already asserted by
`RJdkLogging.formattedOutputIsRealBytes`.

## 3. RETRACTED: the `lib.rs` `Supplier` conveniences were already fixed (#44)

W7-25 §4 closes with "`finest(Supplier)` and its `info`/`warning`/`fine`/
`severe` siblings are registered from `native-builtins/src/lib.rs:17076-17106`
— **not owned, not fixed**, and still evaluate a suppressed supplier", and §7
carries it as an open row. **That is stale, and it was stale when it was
written.** Measured on all nine overloads, logger at `FINE`, counting supplier
invocations — the three columns are identical:

| call | HotSpot | `--real-jdk` | `--jdk-only` |
|---|---|---|---|
| `log(INFO, sup)` | 1, delivered | 1, delivered | 1, delivered |
| `log(FINEST, sup)` | **0** | **0** | **0** |
| `info(sup)` / `config(sup)` / `warning(sup)` / `severe(sup)` / `fine(sup)` | 1, delivered | 1, delivered | 1, delivered |
| `finest(sup)` / `finer(sup)` | **0** | **0** | **0** |

Nine of nine, both modes. **Nothing was changed here to achieve that**, and
nothing should be: the conveniences never had a threshold of their own.
`jul_convenience_log` (`lib.rs:24108`) resolves the `Level` constant and
`invoke_virtual`s straight into `log(Level, Supplier)V`, which the registry
census confirms is `native_jul_logger_log_supplier` at `logmanager.rs:6346` —
**the exact native W7-25 §4 gated**. The fix propagated through the delegation
the same commit relied on for everything else.

The lesson is not that W7-25 was careless; §4's own `Compatible` effect table is
explicitly derived by reading rather than running, and §8 says so. It is that
**the ownership boundary was read as a defect boundary**. `lib.rs:17076-17106`
was correctly identified as unowned, and "unowned" was carried into the open
list as "unfixed" without the one probe that separates them. The probe is nine
lines and it is the first thing this lane ran.

Same shape as: a source-only audit's URGENT row can be stale while its MEDIUM
rows are live. Here the row was stale and the two either side of it were live.

## 4. The vector is correct

Checked against HotSpot, not assumed: **63/63 `PASS RJdkLogging`**, over
repeated runs. Every failing check above is CratonVM diverging from a measured
HotSpot answer, and no check asserts merely that a call returned.

The anti-silence property W7-25 §5 built the vector around is intact and is
still load-bearing: `formattedOutputIsRealBytes` drives a real `StreamHandler`
over a real `SimpleFormatter` into a `ByteArrayOutputStream` and asserts on the
bytes, and it is precisely that check (#59) which catches the source-inference
gap. A version of this vector that asserted only "the logger did not throw"
would be green today in both modes, against a VM whose formatted output names
the wrong thing.

One correction, not to the vector but to how §0's transcript should be read:
`CK RJdkLogging supplier=evaluated:1` is the PASSING value. A future reader
diffing transcripts should compare against HotSpot's, which prints the same
line, rather than against the intuition that a supplier evaluation count above
zero is a bug.

---

## 5. Out-of-file patch (not applied) — `native-builtins/src/phases_early.rs`

**Both remaining `--jdk-only` failures are the same defect W7-25 §1 found, at
sites §6.1 did not cover.** W7-25 generalised it correctly and then patched two
rows.

The `java/util/logging/` shadow retirement takes the registry from **223 rows
in `Compatible` to 66 under `--jdk-only`**. Every honest `Bridge` and
`synthetic-stub` is refused. What survives, from
`--jdk-only --dump-native-registry`, is **30 `java/util/logging` rows, all of
them `kind=intrinsic`, all of them registered by `phases_early.rs` between
lines 20257 and 20527** — inside `register_phase54_logging_extras`, whose head
sets the ambient category:

```text
phases_early.rs:20216  pub(crate) fn register_phase54_logging_extras(...) {
phases_early.rs:20218      r.set_category(NativeKind::Intrinsic);      <-- ambient, 395 lines
phases_early.rs:20592      r.with_category(NativeKind::Bridge, ...)    <-- W7-25 §6.1, the LogManager tail
phases_early.rs:20613      r.set_category(__prev_cat);
```

`Intrinsic` is exempt from the retirement. W7-25 §6.1 lifted the two
`LogManager` rows out of the ambient block; the other 30 are still in it. The
surviving set is the **entire `LogRecord` accessor surface** (25 rows, including
its `<init>`), four `Handler` methods, and `Formatter.formatMessage`.

Two of them are measurably not intrinsics, by W7-25's own definition — *an
intrinsic cannot give an answer the bytecode would not*:

| row | registered | body | JDK 25 bytecode | measured |
|---|---|---|---|---|
| `Formatter.formatMessage(LogRecord)String` | `phases_early.rs:20525` | `invoke_virtual(rec, "getMessage")` and return it | resolves the bundle, then `java.text.MessageFormat.format` when the message contains `{n}` and parameters are present | HotSpot `one=A two=B`, CratonVM `one={0} two={1}` — **#54, both modes** |
| `LogRecord.getSourceClassName()String` | `phases_early.rs:20331` | `lr_get(ctx, args, "sourceClassName", 2)` — a bare field read | `if (needToInferCaller) inferCaller(); return sourceClassName;` | HotSpot `PWalk`, CratonVM `null` — **#59's `--jdk-only` half** |

The `getSourceClassName` row is the cleaner illustration: the intrinsic is the
real getter **with the `inferCaller()` call deleted**. It returns the field, and
the field is null because nothing ever inferred it. `getSourceMethodName`
(`:20343`) is the same, and `LogRecord.<init>(Level,String)V` (`:20257`) is the
third leg — the real constructor is what sets `needToInferCaller = true`, so
even a correct getter would find nothing to do.

This is why #59 is not one defect but two with one symptom. In `Compatible` the
frames are missing and the record is ours (§2 fixes it). In `--jdk-only` the
frames are byte-identical to HotSpot's (§2's table) and the record is real —
and the answer is still null, because four intrinsics stand between the real
record and its real accessors.

### The patch

Retire the group the same way §6.1 retired the `LogManager` pair — by
registering it as what it is, so the retirement can see it. The minimal
evidence-backed set is `Formatter.formatMessage` plus the `LogRecord`
`<init>` / `get`+`setSourceClassName` / `get`+`setSourceMethodName` five:

```rust
    r.with_category(cratonvm_native_api::NativeKind::Bridge, |r| {
        // NOT `Intrinsic`, for the reason the LogManager pair below is not:
        // these give answers the bytecode would not. `formatMessage` returns
        // the RAW pattern where the bytecode runs MessageFormat over it
        // (measured: HotSpot `one=A two=B`, here `one={0} two={1}`), and the
        // source-name accessors are the real getters with `inferCaller()`
        // deleted, so they answer null on a stack HotSpot resolves to
        // `Class method`. `Intrinsic` is exempt from the java/util/logging
        // shadow retirement, so under `--jdk-only` these rows survive alone
        // in front of real bytecode that is already correct. See
        // docs/known-issues/jdk-only/W7-35-jul-supplier-and-payload-residuals.md.
        //   ... the existing `LogRecord.<init>`, get/setSourceClassName,
        //       get/setSourceMethodName and Formatter.formatMessage
        //       `r.register` calls, bodies unchanged ...
    });
```

Only the `r.register` calls move inside the closure; no body changes.

Both bytecode claims in the table above are read off
`javap -p -c --module java.logging`, not from memory. `getSourceClassName` is
literally `getfield needToInferCaller / ifeq / invokevirtual inferCaller /
getfield sourceClassName / areturn`. `formatMessage` is `getMessage`, then
`getResourceBundle().getString(...)`, then `getParameters()`, then an
`indexOf`/`charAt` scan for a `{n}` and `MessageFormat.format`.

> **CORRECTION, 2026-08-12 — the diagnosis above was right and this patch was
> not.** Moving the row into a `with_category(Bridge)` block LANDED, and #54
> stayed red in both modes. The `java/util/logging/` retirement is not driven
> by a row's kind: it is the explicit `RETIRED_SHADOW_TRIPLES` table in
> `native-api/src/retired_shadow.rs` that re-tags a triple to `SyntheticStub`,
> and `Formatter.formatMessage` was never added to it. `Bridge` makes a row
> *eligible* to be listed; it does not list it. And listing it would have
> closed `--jdk-only` only — a `SyntheticStub` still dispatches in
> `Compatible`, where #54 also fails. #54 is FIXED by giving the row a correct
> body instead: W7-43-formatmessage-substitution.md.

**The retired `formatMessage` calls back into rows this set leaves alone**, and
that is checked rather than assumed: it reads `getParameters()` and
`getResourceBundle()`, both still `phases_early.rs` intrinsics. They answer
correctly on a real record — `lr_get` guards on `log_record_real_layout` and
reads by field name — and the vector already proves it, since `recordPayloads`'
`getParameters()` checks (length 2, values `"A"`/`"B"`) pass under `--jdk-only`
today. So the minimal set does not strand its own dependencies.

**Pair the setters with the getters.** `inferCaller()` writes its result
through `setSourceClassName`/`setSourceMethodName`; retiring the getters while
leaving the setters as intrinsics leaves the inference writing through a
fabricator into the field the now-real getter reads. It would very likely still
work — `lr_set` guards on `log_record_real_layout` and writes by name for a
real record — but it is a coupling nobody needs, and the setters are no more
intrinsic than the getters.

**`Compatible` is untouched by all of it.** These rows are not overwritten by
anything (`overwrote=None` on every one), the retirement only refuses under
`--jdk-only`, and a category is not a dispatch decision.

**The 24 rows deliberately NOT in the minimal set**, and why they are still a
problem: the rest of the `LogRecord` surface (`get`/`setMessage`, `getLevel`,
`getMillis`, `getThrown`, `getParameters`, `getLoggerName`, the sequence and
thread accessors) and `Handler.setLevel`/`getLevel`/`flush`/`close` are in the
same ambient block and are no more intrinsic than the five above — they are
stand-ins for bytecode. None of them is currently failing a check: the
`handlerLevelIsASecondGate` and `recordPayloads` sections pass in strict on
every assertion except the two named. **That is an argument for measuring them,
not for leaving them mislabelled.** The honest end state is that
`register_phase54_logging_extras` does not set a function-wide `Intrinsic` at
all — W7-25's own generalisation — but flipping 30 rows' retirement behaviour at
once moves `BASELINE_SYNTHETIC_STUBS` and `bridge_shadows_bytecode` together and
needs its own measured run on the platform those ratchets are keyed to. It
should be a wave item, with the census diffed per triple in both modes, and not
a rider on this one.

### Verification that must accompany it

1. `--jdk-only --dump-native-registry` — the `Formatter.formatMessage` and
   `LogRecord.getSourceClassName` rows must be **absent**, not merely
   re-kinded. Today both are present as `intrinsic`.
2. `RJdkLogging` under `--jdk-only` must reach `PASS RJdkLogging (63 checks)`.
   #54 and #59 are its last two failures.
3. Re-take the census in both modes and diff the surviving
   `java/util/logging/` rows per triple. Rows move `Intrinsic` → `Bridge`, so
   `BASELINE_SYNTHETIC_STUBS` and `bridge_shadows_bytecode` both move; re-freeze
   from one real run on `25/linux`, which is not this lane's platform.

---

## 6. What is proven and what is not

**Proven by running the pre-built binary** (HotSpot 25.0.3+9 control on every
arm, every measurement taken **before** this lane's source changes):

* the three-arm 63-check census and the hard vector's stopping point (§0);
* `useParentHandlers` delivering to the parent after an explicit `false` in
  `Compatible`, and not in strict or on HotSpot (§1);
* the rendered `SimpleFormatter` output and the three-arm `StackWalker` frame
  lists taken from inside a `Handler.publish` (§2);
* all nine `Supplier` overloads' evaluation counts and delivery, three arms
  (§3);
* the registry census in both modes — 223 vs 66 `java/util/logging` rows, the
  30 surviving `intrinsic` rows and their `registered_by` lines, and the
  absence of every `java/util/logging/Logger` triple under `--jdk-only` (§1,
  §5).

**Not proven, and not claimed. Nothing was rebuilt.**

* Neither §1 nor §2 was run. Both are reasoned from a measured defect and, in
  §1's case, from the fix W7-25 §7 had already specified. The predicted effect
  is `--real-jdk` 3 failures → 1 (#54 alone) and `--jdk-only` unchanged at 2.
* §5's patch was not applied, and the prediction that it takes `--jdk-only` to
  63/63 has not been run.
* §2's cost claim — that a handler-less logger pays no stack capture — is read
  off the control flow, not measured. No Spring Boot or Tomcat suite was run
  against any of this; §2 changes what every formatted JUL line renders in its
  `%2$s` position, from the logger name to `Class method`, which is HotSpot's
  answer and a visible console change.
* No ratchet number here is a measurement.
