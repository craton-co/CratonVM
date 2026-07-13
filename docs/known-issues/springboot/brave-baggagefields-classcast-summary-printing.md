# `BraveAutoConfigurationTests`: `ClassCastException: brave.internal.baggage.BaggageFields cannot be cast to java.lang.String` during JUnit summary printing

**Status: OPEN. NOT a memory-safety issue** — no native crash, no
heap-corruption guard hits (this is a "clean" type confusion — a live,
valid Java object of the wrong type, not corrupted/garbage memory). Root
cause is narrowed to a specific mechanism (`java/util/Collections`'s
`<clinit>` silently failing to run to completion during very-early VM
bootstrap, long before Brave/CLDR code executes), but the exact reason
`<clinit>` fails/no-ops has NOT been found. Extensively investigated
2026-07-13 (see "Investigation trail" below) — reproduces 100%,
`-Parallel 1`, both `-Jit on` and `-Jit off` (JIT ruled out).

Found while verifying the [`OnClassCondition` NPE-cast-to-`String[]`
fix](../../internal/springboot/onclasscondition-npe-cast-string-array-cluster-FIXED.md).
`BraveAutoConfigurationTests` used to `FAIL` normally (26 tests, 2 failed,
~230s) on unmodified `dev`. With that fix applied (which corrects
`@ConditionalOnClass` evaluation, changing which beans/conditions match for
this class), the run instead dies after ~30-70s before printing any test
results:

```
WARN cratonvm_vm::vm::vm_util: <clinit> failed — wrapping in ExceptionInInitializerError
  class=sun/util/cldr/CLDRBaseLocaleDataMetaInfo
  cause=java/lang/ClassCastException brave.internal.baggage.BaggageFields cannot be cast to java.lang.String
  ...
Caused by: java/lang/ClassCastException: brave.internal.baggage.BaggageFields cannot be cast to java.lang.String
	at SbRunner.main(SbRunner.java:39)
	at org/junit/platform/launcher/listeners/MutableTestExecutionSummary.printTo(MutableTestExecutionSummary.java:156)
	at java/io/PrintWriter.printf(PrintWriter.java:870)
	at java/io/PrintWriter.format(PrintWriter.java:973)
	at java/util/Formatter.format(Formatter.java:2761)
	at java/util/Formatter$FormatSpecifier.print(Formatter.java:3155)
	...
	at java/text/DecimalFormatSymbols.getInstance(DecimalFormatSymbols.java:181)
	at sun/util/locale/provider/LocaleProviderAdapter.getAdapter(LocaleProviderAdapter.java:251)
	...
	at sun/util/cldr/CLDRLocaleProviderAdapter.<clinit>(CLDRLocaleProviderAdapter.java:54)
	at sun/util/cldr/CLDRBaseLocaleDataMetaInfo.<clinit>(CLDRBaseLocaleDataMetaInfo.java:19)
```

## Investigation trail (2026-07-13) — exact mechanism found, root cause not yet found

All of the following is confirmed via direct instrumentation (temporary,
reverted — see "How to reproduce this trail" below), not guessed.

**1. `--nojit` bisection: not JIT.** Identical crash, same stack, same
message, with `-Jit off`. Rules out JIT codegen entirely.

**2. Exact failing bytecode site, via `CRATONVM_DBG_CCE=1`** (interpreter's
existing `[CCE_DBG] checkcast fail` trace, `vm/src/runtime/interpreter.rs`
~line 14332): the failing `checkcast java/lang/String` is at bytecode
offset 28 inside `sun.util.locale.InternalLocaleBuilder.setLanguageTag
(LanguageTag)` (`InternalLocaleBuilder.java:351`), reading
`langtag.extlangs().get(0)`. `sun.util.locale.LanguageTag` is a `record`
with 7 fields (`language, script, region, privateuse, extlangs, variants,
extensions`); the interpreter's own `[CLINIT-TRACE]` dump confirms the
`Locale.forLanguageTag` call that reaches this is
`CLDRBaseLocaleDataMetaInfo.<clinit>` bytecode offset 22 — its **very
first** locale parse, `Locale.forLanguageTag("en-001")`, confirmed via
`javap` disassembly of the real bundled JDK 25 class.

**3. Dumping the `LanguageTag` record's fields**: `extlangs`, `variants`,
and `extensions` (fields 4/5/6) are all the **same `ObjectRef`** — this is
CORRECT, expected real-JDK behavior, not a bug: `javap`-disassembling
`LanguageTag.parseExtlangs`/`parseVariants`/`parseExtensions` confirms all
three return the class's own `EMPTY_SUBTAGS` static field
(`private static final List<String> EMPTY_SUBTAGS = Collections.emptyList();`,
set once in `LanguageTag.<clinit>`) when nothing of that kind is found —
exactly the case for `"en-001"` (no extlangs/variants/extensions). This
was initially mistaken for a bug (the original hypothesis in this doc's
prior revision); it is not.

**4. Dumping that shared object**: `cid=63, class=java/util/ArrayList,
real_total_fields=3`, fields `[Int(0), Object(<real non-null array ref>),
Int(1)]` — i.e. a **normal, correctly-shaped, live `ArrayList` with one
real element** (the `BaggageFields` instance), not corrupted/garbage
memory. This ruled out (in order, each with a build+test cycle):
   - **Field-index mismatch in `Collections.emptyList()`'s native
     override** (`native-builtins/src/phases_early.rs` — hardcodes
     `elementData=field 0, size=field 1`, but real `java/util/ArrayList`
     has `AbstractList.modCount` at field 0, shifting `elementData`/`size`
     to 1/2). Plausible-looking but **wrong**: confirmed via an
     unconditional debug print that this native is **never called at all**
     in real-JDK mode — `register_core_stdlib_extras` (which registers
     `emptyList`/`emptyMap`/`emptySet`) is only reachable through
     `register_enterprise_final_natives`, itself gated to
     **synthetic-jdk mode only** (see the `register_formatter_natives`
     comment a few lines above it in `lib.rs`) — dead code for this test.
     This matches the known [[reference_synthetic_jdk_dead_registration_trap]]
     pattern. A fix attempt here (singleton-caching `emptyList`/`emptyMap`/
     `emptySet` via the real `Collections.EMPTY_LIST`/`EMPTY_MAP`/`EMPTY_SET`
     static fields, mirroring the working `emptyIterator()`/`EMPTY_ITERATOR`
     pattern a few lines below in the same function) was written, built,
     and tested — **had zero effect** (confirming the dead-code diagnosis)
     and was reverted.
   - **A genuinely non-empty `extlangs` parse** (i.e. `"en-001"` somehow
     matching `isExtlang`/`isVariant`) — ruled out by `javap`-disassembling
     `parseExtlangs`/`parseVariants`: both correctly fast-path to
     `EMPTY_SUBTAGS` for this input; the real bytecode logic is sound.
   - **`Collections$EmptyList` class-identity aliasing with `ArrayList`**
     (i.e. `cid=63` secretly being both) — ruled out: `resolve_class_loader_aware`
     for `"java/util/Collections$EmptyList"`, called directly from the
     failure site, resolves cleanly to a **new, never-before-seen**
     `ClassId(4188)` — the class loads fine when actually attempted; it
     had simply never been touched.

**5. The actual finding**: at the crash point, `java/util/Collections`
(`ClassId(70)`) has `class.state == ClassState::Initialized`, with no
`initializing_thread` — i.e. the VM believes `Collections` is **fully,
successfully initialized** — yet `java/util/Collections$EmptyList` has
**never been loaded** (`get_loaded_class_id` returns `None`). Real
`Collections.<clinit>` bytecode's first four instructions are
`new Collections$EmptyList(); dup; invokespecial <init>; putstatic
EMPTY_LIST` (confirmed via `javap`) — if that ran, `Collections$EmptyList`
would necessarily be loaded. **It provably is not.** Separately confirmed
`has_clinit=true` for `Collections` (the VM does detect the real `<clinit>`
method) and the class-init trigger for `Collections` fires exactly **once**,
extremely early — interleaved between the `Unsafe`/`BigInteger`
"Post-clinit fixup" bootstrap log lines, ~27 seconds before the crash, i.e.
during core VM bootstrap, long before any Brave/Spring code runs.
`CRATONVM_DBG_CATALINA=1` (which prints every swallowed `<clinit>`
exception for `java/util/*` classes, among others) shows **nothing** for
`Collections` — so if `<clinit>` is failing, it is not going through the
"lenient mode" swallow-and-log path at `vm_util.rs` ~line 1443 that most
other classes' swallowed `<clinit>` failures go through.

**Net effect**: `Collections.EMPTY_LIST` (and very likely `EMPTY_MAP`/
`EMPTY_SET`/every other static field `Collections.<clinit>` would set) is
left holding whatever was in that static-storage slot *before* real
`<clinit>` ran — which this run shows was **not zero/null but a stale,
live, valid `ArrayList` reference that some unrelated Brave code
populated**, i.e. either uninitialized static storage isn't reliably
zeroed, or `Collections`'s static-field slot range overlaps/aliases
something else's memory. `Collections.emptyList()` (real bytecode:
`getstatic EMPTY_LIST; areturn`) then faithfully returns that stale value,
which `LanguageTag.<clinit>` caches forever in its own `EMPTY_SUBTAGS`
field, and the very first real use of it (`"en-001"`, the first locale tag
CLDR ever parses) surfaces the BaggageFields.

## What's NOT yet known

- **Why** `Collections.<clinit>`'s bytecode doesn't reach (or doesn't
  successfully complete) `new Collections$EmptyList()`, despite
  `has_clinit=true`, no swallow-log firing, and the class ending up marked
  `Initialized` regardless. Candidates not yet checked: a bootstrap-order
  dependency failure specific to this very-early point in VM startup (this
  triggers earlier than almost anything else observed, interleaved with
  the `Unsafe`/`BigInteger` native-constant fixups); a separate
  swallow/finalize path (there are at least 5 `finalize_init(...,
  ClassState::Initialized)` call sites in `vm_util.rs`, only one of which
  was examined in detail); or `invoke_on_class_shared` itself silently
  short-circuiting for some structural reason specific to this very large
  method (`Collections.java`'s static initializer is one of the largest in
  `java.util`).
- Whether static field storage is generally not zero-initialized (would be
  a significant, broadly-impactful bug on its own), or whether this is
  specific to how/when `Collections`'s static-field range gets allocated
  relative to other early-bootstrap allocations.
- Whether other classes are affected the same way (any class whose
  `<clinit>` first-touch happens to land in this same very-early bootstrap
  window is a candidate for the identical failure mode).

## How to reproduce this trail

The diagnostics used above are almost entirely *existing* env-gated
instrumentation already in the codebase — no rebuild needed for most of
this:
- `CRATONVM_DBG_CCE=1` — checkcast-failure trace (`env_cache::cce_dbg`,
  `interpreter.rs` ~14332).
- `CRATONVM_DBG_CATALINA=1` — swallowed-`<clinit>`-exception trace for
  `java/util/*`/Tomcat-ish prefixes (`vm_util.rs` ~1448).
- `-Jit off` (suite runner) / equivalent CLI flag for the interpreter-only
  bisection.

The remaining steps (dumping `LanguageTag`'s own fields, the shared
`extlangs` object's fields, `Collections`/`Collections$EmptyList`
class-manager state, and `has_clinit`) required temporary `eprintln!`
instrumentation directly in `vm/src/runtime/interpreter.rs`'s `Checkcast`
handler and `vm/src/vm/vm_util.rs`'s class-init path — all reverted after
use; re-add similarly gated prints (or promote them to a permanent,
env-gated diagnostic) to continue from here.

## Repro

```powershell
apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 -SpringBootRoot C:\craton\CratonVM\apps\spring-boot `
  -ClassList <TSV row: module/spring-boot-micrometer-tracing-brave	org.springframework.boot.micrometer.tracing.brave.autoconfigure.BraveAutoConfigurationTests> `
  -Start 1 -Count 1 -Exe <cratonvm exe from dev, post onclasscondition-fix merge>
# add `-Jit off` — reproduces identically, confirmed JIT-independent.
```

## Related

- [[reference_synthetic_jdk_dead_registration_trap]] — same
  dead-registration shape as the `Collections.emptyList()` native override
  ruled out in step 4 above (unrelated to this bug's actual root cause, but
  a real trap worth knowing about independently).
- Not the same corruption signature as
  [`onclasscondition-npe-cast-string-array-cluster-FIXED.md`](../../internal/springboot/onclasscondition-npe-cast-string-array-cluster-FIXED.md)'s
  `java/lang/String`/`ClassId(6)` heap corruption (fixed, `e7e3bb91f`) — no
  `gen_heap::` guard fires here; this is a clean, valid, wrong-typed live
  object, not memory corruption.
