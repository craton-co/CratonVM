# `LoggingSetupRecorder.initializeLogging` `NoSuchMethodError` — FIXED 2026-08-13; the page was written against a 5-day-stale binary

**Status: FIXED and landed on `dev` as `70248949c` (2026-08-13 15:21 -0300),
four days BEFORE this page was opened.** Re-measured and retired 2026-09-01.

The original page (2026-08-17) reported this as `OPEN, not yet root-caused`
and spent its whole investigation on the class file and on CratonVM's
descriptor parsers. Both were innocent. The malformed descriptor never came
from a class file at all: it was a **literal written down inside CratonVM's
own Quarkus logging native**, and the run that produced it used a
`cratonvm-quarkus-zgc` binary built on 08-12 — before the fix landed on
08-13. Everything below the first three sections is the original
investigation, kept because its negative results are what make the real cause
legible.

## What it was

Every quarkus test class died before any `@Test` method started:

```
NoSuchMethodError method="io/quarkus/runtime/logging/LoggingSetupRecorder.initializeLogging(Lio/quarkus/runtime/logging/DiscoveredLogComponents;Ljava/util/Map;ZLio/quarkus/runtime/RuntimeValue;Ljava/util/List;Ljava/util/List;Ljava/util/List;Ljava/util/List;Ljava/util/List;Ljava/util/List;Lio/quarkus/runtime/RuntimeValue;Lio/quarkus/runtime/LaunchMode;Z)Lio/quarkus/runtime/shutdown/ShutdownListener;" caller="io/quarkus/test/config/LoggingSetupExtension.<init>()V @pc=7"
```

Thirteen parameters, six `Ljava/util/List;`. The real method has fourteen and
seven. `LoggingSetupExtension` is a global JUnit5 `Extension` every quarkus
test class registers, and its constructor calls
`LoggingSetupRecorder.handleFailedStart()` — so this fired on the first class
of every forked VM, and the harness recorded `found>0, started=0` for all
6,377 classes. That is also the defect behind the earlier, vacuous "quarkus
3,939/3,939 PASS (100%)": every one of those classes had `started=0`.

## Root cause: a descriptor CratonVM wrote down, and a class that moved

`handleFailedStart` is not interpreted. `native-builtins/src/phases_late.rs`
carries a native mirror for both overloads
(`native_quarkus_logging_handle_failed_start`, registered in
`phases_late/nio_file.rs`), added for Keycloak: the real recorder builds a
transient SmallRye config that, under CratonVM, reaches mapping validation
without Quarkus' `Charset`/`MemorySize` converters. The mirror rebuilds that
flow with the converters made explicit — and it ended by calling

```rust
ctx.invoke_virtual(recorder, "initializeLogging", "<a 13-parameter literal>", &[…13 values…])
```

The literal matched the Quarkus revision the native was written against.
Quarkus has since grown a seventh `List` parameter (the per-`NamedHandlerType`
formatter map), so the literal named a method that no longer exists. Nothing
about the application's bytecode, its class files, or CratonVM's descriptor
parsing was involved — which is exactly why all five of the original checks
below came back clean.

`70248949c` replaced the literal: the descriptor is now **read off the class**
and the argument vector filled from its parsed parameter list, so either shape
works. That commit is titled `fix(classpath): getResources re-derived the whole
classpath per probe, 136x HotSpot's syscalls` and carries this fix as its last
paragraph — it is not findable by grepping for anything this page says.

## Why it read as "only at real classpath scale"

The page's central open question was whether classpath *entry count* is the
variable. It is not. The mirror has a long prelude —
`ConfigProvider.getConfig()`, `unwrap(SmallRyeConfig)`, a fresh
`SmallRyeConfigBuilder`, `addDiscoveredConverters()`, the config-mapping
registration — and it only reaches the `initializeLogging` call if all of that
succeeds. A classpath big enough to satisfy that prelude reaches the bad
literal; a smaller one dies earlier, somewhere else. Measured on the 08-12
binary, 2026-09-01:

| classpath | outcome on the 08-12 binary |
|---|---|
| 2 entries (repro dir + `core/runtime/target/classes`) | `class not found: org/eclipse/microprofile/config/ConfigProvider` — the mirror never reaches the descriptor |
| 4,230 entries (`common.args`) | the 13-parameter `NoSuchMethodError`, 3/3 |

That is also the whole explanation for the "genuine trap for future subsetting
attempts" the page recorded: every subset that dropped a provider surfaced a
*different* `NoClassDefFoundError` instead of a clean pass/fail, because the
subsetting was walking the mirror's own dependency chain, not the
application's.

## Verification, 2026-09-01

Azure host, the preserved binaries, the harness's own argfiles, unmodified.
`MiniDriver` is the page's own one-line driver
(`LoggingSetupRecorder.handleFailedStart()`; prints `MINI_DRIVER_OK`).

| binary | built | `common.args` (4,230 entries) | `common.args.orig-before-logmanager-fix-20260817` |
|---|---|---|---|
| `cratonvm-hibreactive-zgc` (same 08-12 tree as the binary the page ran) | 08-12 01:06 | **`NoSuchMethodError`, 13 params** | — |
| `cratonvm-quarkus-default` | 08-17 13:33 | `MINI_DRIVER_OK` | `MINI_DRIVER_OK` |
| `cratonvm-quarkus-zgc` | 08-17 16:35 | `MINI_DRIVER_OK` | `MINI_DRIVER_OK` |

* The `-Djava.util.logging.manager=…` line the sibling page added to
  `common.args` is **not** the variable: both argfiles give the same answer on
  both binaries.
* `--nojit` on the 08-12 binary still fails, identically — this was never a
  JIT defect. It also explains the odd caller frame the page quoted: the
  mirror is a native, so there is no `handleFailedStart` interpreter frame for
  the reporter to name, and the frame it prints is the caller's caller.
* A real quarkus class through the harness on the 08-17 binary
  (`io.quarkus.aesh.deployment.AeshContextTest`, `CratonRunner`, 4,230-entry
  classpath): **zero** `NoSuchMethodError` occurrences anywhere in the log.
* Windows, dev tip of 2026-09-01, `MiniDriver` on a locally rebuilt 308-entry
  and a 2,226-entry classpath (all of `.m2` plus every built quarkus
  `target/classes`): `MINI_DRIVER_OK` on both, HotSpot 25 agreeing.

The three copies of `LoggingSetupRecorder.class` on the real classpath
(`core/runtime/target/classes`, `quarkus-core-999-SNAPSHOT.jar`, and the
`quarkus-cli-999-SNAPSHOT-runner.jar` uber-jar the original page did not know
about) are **md5-identical** and unchanged since 08-12, so a duplicate-class
skew is ruled out by measurement rather than by reading.

## What the original page checked, and why every check was clean

Kept, because these are the results that point at the native:

1. **`javap` on the real jar** — exactly one `initializeLogging`, 14
   parameters, 7 of them consecutive `List`, returning `ShutdownListener`.
2. **The module's compiled `target/classes` copy** — byte-identical (`cmp`,
   50065 bytes) to the jar copy. No version skew between what the caller was
   compiled against and what is on disk.
3. **Disassembling the caller** — `handleFailedStart(RuntimeValue)`'s
   `invokevirtual` at bytecode offset 167 names the same 14-parameter
   descriptor.
4. **The descriptor parsers** — `classloading/src/vtype.rs`
   (`param_types_from_descriptor`, `field_descriptor_len`),
   `reader/src/method_descriptor.rs` (`MethodDescriptor::parse`),
   `reader/src/field_type.rs` (`parse_partial_depth`) — read in full,
   structurally sound, and hand-written repros against them did not reproduce.
5. **Every `*_CACHE_CAP`/FIFO-bounded cache** —
   `classloading::class_path::CANONICALIZE_CACHE_CAP`,
   `vm::native::jni::DESCRIPTOR_CACHE_CAPACITY`,
   `jit::helpers::VIRTUAL_TARGET_CACHE_CAP` — surveyed and none implicated.

All five are correct. The one place not looked at was CratonVM's own native
mirror of the method being called, and the tell was in the page's own data:
repros 1 and 2 (a static and an instance method with the same 7-`List` shape)
passed, while repro 3 — **the real method** — failed. The shape was never the
variable; the identity of the method was.

## Residual closed 2026-09-01: the mirror had no output of its own

The page's remaining ask was "instrumented tracing at the resolution call site
showing what `descriptor` looked like at each step". The useful version of that
sits upstream of the resolution: **the mirror was silent**. Every refusal path
in `native_quarkus_logging_handle_failed_start` is a bare `return Ok(None)`,
which for a registered native means "handled, void" — so a rotted signature
produced no output at all, and the only symptom was `started=0` on every
quarkus class, with nothing anywhere saying a hand-written native was what
named a method that does not exist. That is what cost five days, and it is
what would have cost the next five.

Landed with this retirement, in `native-builtins/src/phases_late.rs`:

* **`logging_setup_arg_plan`** — the argument-vector decision extracted as a
  pure function of the descriptor, unit-pinned against **both** shapes: the
  six-`List` one the literal used to name and the seven-`List` one quarkus
  999-SNAPSHOT ships. Arity alone is not enough of a check, so the tests also
  pin that the handler `RuntimeValue` is `null` and that only the LAST one
  carries the supplier — a swap of those two survives any arity assertion.
* **Refusals reached after the recorder class resolves now WARN once**, naming
  the parameter that could not be filled. Refusals *before* it stay silent:
  those only mean "this application is not Quarkus".
* **The constructor descriptor is still a literal** — it has no second shape
  to derive from — so a mismatch is now named before the invoke, and the
  `NoSuchMethodError` that follows is left exactly as it was. Turning it into
  a silent no-op would trade a loud failure for a quiet one.
* **An INFO-level engagement census on the success path**, because a passing
  run cannot otherwise tell "the mirror served this" from "the mirror never
  ran" — the real bytecode sets logging up too. Costs nothing behind the
  WARN-level default filter; one `RUST_LOG=cratonvm_native_builtins=info`
  away.

`cargo test -p cratonvm-native-builtins --lib`: 4184 passed, 0 failed (4178
before, +6 new).

## Fast repro (kept, and corrected)

```bash
cd apps/quarkus-suite-runner
cp repro-lsr-nsme/MiniDriver.java .
javac @<the -cp lines of common.args> -d . MiniDriver.java   # the classpath
                                            # exceeds argv limits — use @argfile,
                                            # not a literal -cp on the command line
CV_BIN=$(pwd)/../../target-zgc/release/cratonvm-quarkus-zgc
"$CV_BIN" -XX:+UseZGC @common.args MiniDriver
```

Oracle: `java @common.args MiniDriver` — `MINI_DRIVER_OK` in ~1s.

**Rebuild the binary first.** The original page's failing run took its binary
from `cratonvm-quarkus-zgc-wrapper.sh`, whose target had last been built five
days earlier; the same wrapper path, rebuilt, passes. On this host a wrapper
script is no evidence of when the binary behind it was compiled.

## Related

- `julogger-cast-to-jbosslogmanager-logger-20260817.md` — the next blocker in
  the same bootstrap chain, fixed the same week. Its "Related" section calls
  this page "a 5-day stale binary, not a live defect"; the binary was stale,
  but the defect was real and its fix is `70248949c`.
- `quarkustestprofileawareclassorderer-not-a-hang-throughput-gap-20260817.md`
  — the layer after that one, and the page that records `found=N, started=0`
  for these classes reproducing on **HotSpot** too, so a `NOSTART` there is
  not by itself a CratonVM verdict.
- The harness's `NOSTART` classifier (`found>0, started=0, failed=0` used to
  read as `PASS`) lives in `apps/quarkus-suite-runner/run-quarkus-suite.sh`,
  which is untracked host-local tooling, not repository state.
