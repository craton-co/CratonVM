# `DateFormatSymbols.getProviderInstance` fails codegen — FIXED

**Status:** ✅ **FIXED 2026-08-02** (`fix/dfs-getproviderinstance-20260802`).
Retired from `docs/known-issues/`. Found 2026-07-31 while re-deriving
[tomcat/32.4](fixed-suite-bugs/tomcat/32-doc04-residual-perf-assertions-CLOSED.md).

The root cause was not specific to `DateFormatSymbols`, or to date
formatting, or even to a JDK class: **the JIT had no representation for
`ldc <Class>` at all.** Every method containing a class literal —
`Foo.class`, and every `getAdapter(SomeProvider.class, …)`-shaped call in
the JDK — was permanently bail-listed and never compiled, at any tier.

## What the bug actually was

`getProviderInstance` is 37 bytes and starts with one:

```
 0: ldc           #60   // class java/text/spi/DateFormatSymbolsProvider
 2: aload_0
 3: invokestatic  LocaleProviderAdapter.getAdapter:(Ljava/lang/Class;Ljava/util/Locale;)…
```

The VM's `cp_ldc_resolver` (three copies, `vm/src/runtime/interpreter/
invoke.rs`) modelled exactly two constant kinds — `Integer`/`Float` as an
immediate, and `String` as UTF-8 bytes materialised at run time through
`helpers.ldc_string`. A `CONSTANT_Class` fell into its `_ => None` arm, and
in `try_compile_inner` a `None` from that resolver is the *permanently
unrepresentable* answer: it sets `backend_attempted`, which puts the method
on the bail-list forever (RBC.7, added so that `ldc "str"` methods would
stop re-running the whole upgrade gauntlet every `JIT_RETRY_STRIDE` calls).
Correct for a `MethodHandle`; wrong for a class literal, which is ordinary
Java that any real workload contains.

The same hole existed on the two paths that call the x64 backend directly:
the early-compile path in `interpreter.rs` additionally inserted such
methods into `jit_skip_set`, which then blocked the hot-path compile as
well, and `compile_osr_artifact` refused the whole OSR artifact.

## Why nothing named it, and what does now

The doc's original complaint — *"Nothing names why, which is the first
thing to fix"* — was accurate, and the reason is worth keeping:
`backend_attempted` looks like a classification but is really a
**permanence** flag. Three constant-pool resolver misses set it precisely
so the method lands on the bail-list, so `backend_attempted=true` was
printed both by "codegen has a hole" and by "this CP entry is a shape the
compiler never accepts". One bit cannot carry two questions.

Three things now name the reason instead:

* `try_compile` records a per-compile bail site (`JIT_BAIL_SITE`,
  `jit/src/lib.rs`), set by every resolver bail, every permanent bail, and
  the single-pass backend's per-opcode refusal (with the pc and opcode).
  `CRATONVM_DBG_JITC=1` prints it: `compile-bail … reason=<site>`.
* A new `jitc_permanent_bail!` macro makes the three permanent sites
  announce themselves as such rather than silently setting a flag.
* The reason outlives the compile in a per-method store, so
  `CRATONVM_DBG=jit-method-stats` — the table where a permanently
  uncompilable hot method is actually *noticed*, long after the compile
  worker has moved on — prints `reason=` on every stuck entry.

That last one also corrected a false claim: the stats table headed its
compile-failure list *"not policy — these are bugs"*, but `ineligible`
covers only the tier manager's own declines, so a deliberate correctness
gate inside the compiler landed there looking like a defect. All three
methods still failing on this path are exactly that (see below).

With the reason wired, the diagnosis took one run:

```
[cratonvm-jitc] permanent-bail site=ldc-constant-unsupported java/text/DateFormatSymbols.getProviderInstance(…)
[cratonvm-jitc] compile-bail java/text/DateFormatSymbols.getProviderInstance(…) backend_attempted=true reason=ldc-constant-unsupported
```

## The fix

`ldc <Class>` compiles, using the same shape the cold-`new` fix uses
([jit-compile-bail-unresolved-new-cold-class.md](jit-compile-bail-unresolved-new-cold-class.md)),
and for the same two reasons: the mirror is an ordinary heap object that a
relocating collector can move between two runs of one compiled body, and
the target class may not be loaded when the method compiles — loading it
would mean running a user `ClassLoader.loadClass` from inside the compiler.

* `JitLdcConstant::ClassMirror { holder_class_id, cp_idx }` — the resolvers
  report the *site*, never a resolved class id or an `ObjectRef`.
* `helpers.ldc_class_cp` (new, ABI revision 4, 63 slots) →
  `jit_ldc_class_cp(vm_ptr, holder_class_id, cp_idx)`, which resolves
  through the same loader-faithful `jit_resolve_cp_class` the deferred-`new`
  helper uses and returns `get_or_create_class_mirror`. No `<clinit>` and no
  `new`-style access check — JVMS resolves an `ldc` class reference but does
  not initialise it, matching the interpreter's own `ldc` handler.
* The single-pass backend emits the call, an oop map, and the `0`-return
  pending-exception guard, then pushes the result marked as an oop.
* Wired on all four compile paths: `try_compile`, the callee compile, the
  early-compile path (which no longer poisons `jit_skip_set`), and
  `compile_osr_artifact`.

The helper slot is `OptionalPtr`: a hand-built test table leaving it `0`
makes the backend refuse a class-`ldc` site and bail, which is the pre-fix
behaviour exactly.

## Validation

`probes/LdcClassProbe.java` — four one-instruction methods (a loaded class,
an array class, a class first referenced by the compiled site itself so it
is still unloaded at compile time, and `int.class` as the non-`ldc`
control), 200 000 iterations each with retained allocation churn so young
collections run and relocate. Identity is asserted against references taken
*before* the loop, so a baked-immediate mirror would fail as soon as it
moved. `CRATONVM_DBG_JITC=1` confirms all four methods reach `full-compile`.
Passes on CratonVM and on HotSpot.

`probes/DfsProviderProbe.java` — the doc's own repro, reduced to
`DateFormatSymbols.getInstance(Locale)` (a two-instruction wrapper over
`getProviderInstance`). Before, `getProviderInstance` was the single entry
in the "COMPILE FAILED" list with `tier_fail_count=3`. After, it compiles at
C1, is superseded by C2, and is bound as a direct call; the list no longer
names it.

On `DateFormatPatternProbe`, the doc's own residual list of six goes from
five reported compile failures to three. Fixed by this change:
`DateFormatSymbols.getProviderInstance`,
`CalendarDataUtility.retrieveFieldValueName`,
`NumberFormat.getInstance(Locale, Style, int)`, and
`DateFormatSymbols.initializeData` (a fourth, which the original list did
not name).

Suites: `cargo test -p cratonvm-jit -p cratonvm-jit-api` green (1806 + the
integration fixtures); `cargo test -p cratonvm-vm --lib` green in both
feature configurations (2378/0 plain, synthetic-jdk clean).

## What is left on this path, and who owns it

The three surviving compile failures on `DateFormatPatternProbe` have ONE
named cause between them:

```
sun/util/locale/provider/JRELocaleProviderAdapter.getDateFormatSymbolsProvider()   reason=rbc6-handler-reads-unsafe-local
sun/util/locale/provider/JRELocaleProviderAdapter.getNumberFormatProvider()        reason=rbc6-handler-reads-unsafe-local
java/text/DecimalFormat.format(JLjava/text/Format$StringBuf;…)                     reason=rbc6-handler-reads-unsafe-local
```

Not this bug, and not a defect: RBC.6 is a correctness gate. The two
adapter methods are javac's `synchronized (this) { … }` cleanup-handler
shape, whose handler reads the monitor local, and their protected ranges
contain `getfield`/`putfield` — which are deliberately NOT in
`precise_frame_publishing_opcode`'s admitted set. They were briefly admitted
in `5bf306bb0` (2026-07-28) and removed again after `probes/Rbc6FieldProbe
.java` measured the top-level field arms taking an inline fast path that
neither null-checks nor publishes a frame: a protected `getfield` NPE let
the handler read a non-parameter local as `0` instead of `38`, and a
`putfield` on a null receiver did not throw at all.

That residual now has its own doc:
[known-issues/jit/rbc6-protected-field-ops-block-synchronized-blocks-20260802.md](../known-issues/jit/rbc6-protected-field-ops-block-synchronized-blocks-20260802.md).
Note that `31-synchronized-code-never-jit-compiled-FIXED.md` still claims
`getfield`/`putfield` "are therefore admitted at the relevant precise-frame
sites" — that claim is stale for this shape.

## Reproduction (historical)

```
set CRATONVM_DBG_JIT_METHOD_STATS=1
cratonvm.exe -Xmx2g -Dprobe.iters=4000 -cp <probes-out> DfsProviderProbe
```

## Scale, and the warning that still stands

It is genuinely hot — ~12 000 invocations in a 2 000-iteration run of the
original probe, ~2 per `String.format` call, because `String.format`'s `%t`
conversions resolve locale date symbols.

**Fixing it does not make `TestOneLineFormatterPerformance` pass, and it was
never going to.** That test sits on `String.format`, which is its *fast*
side — the side the assertion races against. Speeding it up makes that test
**harder**. This was fixed because a hot JDK method failing codegen is a
defect worth understanding, and the understanding turned out to be worth far
more than the method: `ldc <Class>` is not rare.

The 2026-07-31 measurement that ruled the six compile-bails out as the cause
of the SLOW side still holds and was not re-derived: `probes/SdfOnlyProbe
.java` runs `SimpleDateFormat.format` with pattern `"ss"`, reaches none of
them, and cost 51 µs with `hot_but_stuck_in_interpreter=0` — 870× HotSpot's
59 ns. The cost there is generic-dispatch round trips; see
[30 § Adopted](fixed-suite-bugs/tomcat/30-hot-loop-jit-admission-bans-testmethodperformance-CLOSED.md#adopted-2026-07-31--two-residuals-from-the-retired-tomcat32-and-where-they-went)
and [raw JIT-to-JIT](jit-raw-jit-to-jit-shadow-stack-overflow-FIXED-20260731.md),
which carry that work. **Do not cite this doc as a lever for any
date-formatting throughput test, on either side.**
