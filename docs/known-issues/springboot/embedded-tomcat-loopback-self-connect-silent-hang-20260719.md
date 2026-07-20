# Embedded-Tomcat integration tests hang indefinitely — root cause is NOT networking, it's a `HashMap`-cycle in Hibernate Validator's generic-type resolution

**Status: OPEN — re-investigated 2026-07-20, `dev` tip `54003fb83`. Root cause
narrowed to a specific, reliably-reproducible trigger; the underlying
mechanism inside CratonVM is still not found. One real (but insufficient)
correctness fix applied and kept: `Class.getTypeParameters()` identity
stability (see below).**

## Correction to the original (2026-07-19) filing

The original version of this doc hypothesized an accept-thread/TCP-readiness
race, and treated this as possibly the same bug family as
`webclient-loopback-self-connect-timeout-os10060-cluster.md`. **Both
hypotheses are wrong.** This session isolated the hang to a completely
different, non-networking cause:

`RemappedErrorViewIntegrationTests` hangs *before* the HTTP self-connect is
ever attempted — during Spring context refresh, inside
`ValidatorAdapter.afterPropertiesSet()` →
`LocalValidatorFactoryBean.afterPropertiesSet()` →
`HibernateValidator.buildValidatorFactory()` →
`ConstraintHelper.forAllBuiltinConstraints()`. The stuck frame (confirmed via
`--stack-dump-on-timeout`, repeatedly, at the exact same bytecode PC) is:

```
org/hibernate/validator/internal/util/TypeHelper.extractConstraintValidatorTypeArgumentType(Class,I)
```

at the bytecode offset of its own `while (map.containsKey(x)) x = map.get(x);`
tail-chase loop (decompiled source, this is NOT CratonVM code — it's real
Hibernate Validator 9.1.0.Final bytecode). `map` is a fresh `java.util.HashMap`
built earlier in the same method call from the validator class's generic
hierarchy (`Class.getGenericSuperclass()` / `getGenericInterfaces()` /
`getTypeParameters()` / `ParameterizedType.getActualTypeArguments()`). CPU
sampling across two 5-second-apart snapshots showed **~2 full cores
continuously busy** the entire time (`+10.5s` CPU per `+5.3s` wall, sustained)
— this is a genuine unbounded **livelock in a Java `HashMap` chase loop**, not
a deadlock, not a parked thread, and not anything network-related. `--nojit`
does not change the outcome (rules out a JIT miscompilation).

This has nothing to do with TCP, `accept()`, or loopback networking at all —
it just happens to occur during the same test-class boot sequence that later
(never reached) would attempt the self-connect. **The relationship to
`webclient-loopback-self-connect-timeout-os10060-cluster.md` claimed in the
original filing does not hold** — that doc's symptom (a real OS-level
`WSAETIMEDOUT`) and this one (an infinite Java-level loop before any socket
is touched) are unrelated bugs that happen to share a superficial "embedded
Tomcat + self-connect test" shape. Treat them as fully independent issues.

## Minimal, fast (~10s) standalone reproduction

No Spring Boot, no Tomcat, no suite runner needed. Built and verified working
against `hibernate-validator-9.1.0.Final.jar` +
`jakarta.validation-api-3.1.1.jar` + `jboss-logging-3.6.3.Final.jar` on the
classpath (versions as resolved into `~/.gradle/caches` by the Spring Boot
checkout):

```java
import java.lang.reflect.Method;
import java.lang.reflect.Type;

public class Main10 {
    static String[] SEQ = {
        "org.hibernate.validator.internal.constraintvalidators.bv.AssertFalseValidator",
        "org.hibernate.validator.internal.constraintvalidators.bv.AssertTrueValidator",
        "org.hibernate.validator.internal.constraintvalidators.bv.number.bound.decimal.DecimalMaxValidatorForBigDecimal",
    };

    public static void main(String[] args) throws Exception {
        Class<?> typeHelper = Class.forName("org.hibernate.validator.internal.util.TypeHelper");
        Method extractValidatedType = typeHelper.getMethod("extractValidatedType", Class.class);
        for (String cn : SEQ) {
            Class<?> c = Class.forName(cn);
            Type result = (Type) extractValidatedType.invoke(null, c);   // real HV algorithm
            System.out.println(c.getSimpleName() + " -> " + result);
            c.getAnnotation(jakarta.validation.constraintvalidation.SupportedValidationTarget.class);
            // ^ this call, on THIS class, in THIS position, is what corrupts
            //   state for the NEXT extractValidatedType call below.
        }
    }
}
```

Run under CratonVM (any recent `dev` build, `--stack-dump-on-timeout 10`):
hangs processing the **third** class (`DecimalMaxValidatorForBigDecimal`),
in `extractValidatedType`, at the exact same `while(containsKey) get()` loop.
Run under real HotSpot: all three resolve instantly and correctly
(`Boolean`, `Boolean`, `BigDecimal`).

This exactly mirrors what `ConstraintHelper`'s real constructor path does per
validator: `ClassBasedValidatorDescriptor`'s private constructor calls, in
order, `TypeHelper.extractValidatedType(validatorClass)` then
`determineValidationTargets(validatorClass)` (which is
`validatorClass.getAnnotation(SupportedValidationTarget.class)` — see
`ClassBasedValidatorDescriptor.<init>` bytecode). `forAllBuiltinConstraints()`
calls this for every one of ~30+ built-in validator families in a fixed
order; `AssertFalseValidator`/`AssertTrueValidator` are among the first two,
`DecimalMax*`/`DecimalMin*` follow shortly after — so the real run hits this
exact 3-class pattern very early, which is why the doc's original "hangs
immediately after context init" symptom shows up so reliably.

## What was ruled out (each independently verified this session)

- **Not a cross-thread race.** Reproduces identically with Spring Boot's
  `BackgroundPreinitializingApplicationListener` disabled
  (`-Dspring.backgroundpreinitializer.ignore=true`) — single-threaded.
- **Not JIT.** `--nojit` does not change the outcome.
- **Not `java.util.HashMap`-specific.** A hand-written reimplementation of
  the exact same algorithm (`resolveTypes`/`resolveTypeForClassAndHierarchy`,
  decompiled from `TypeHelper.class` bytecode and transcribed faithfully —
  see `Main5`/`Main6` in the investigation scratch dir) using a real
  `java.util.HashMap` **or** `LinkedHashMap`, called directly via reflection
  in the exact same class order, **terminates correctly every time** —
  including after forcing ~400MB of GC churn (`System.gc()` × 50 rounds)
  between building the map and chasing it. This rules out a general
  `HashMap`/GC-hash-instability bug as the *sole* mechanism (see below,
  content-verified too).
- **Not `TypeVariable.hashCode()`/`.equals()` instability.** Added
  `CRATONVM_DBG_TYPEVAR_EQ`-gated tracing directly in both natives
  (`native-builtins/src/lang_reflect.rs`): logged every call across the full
  `forAllBuiltinConstraints()` hang and found **zero** hash-value changes for
  any object identity across repeated calls, and **zero** `equals()`
  mismatches.
- **Not stale/corrupted `TypeVariable` object *content*.** Added a
  read-back-and-compare check inside `cached_building_type_parameter`
  (`native-builtins/src/generics.rs`): every cache hit's `name`/
  `genericDeclaration` fields matched what was originally stored, right up to
  the hang. Rules out the "GC moved the object, the cached `ObjectRef` went
  stale" theory that the `var_handle_roots` remap mechanism
  (`vm/src/memory/gc.rs` step 6b, `vm/src/memory/roots.rs` step 8b) was
  designed to prevent — that mechanism appears to be working correctly here.
- **Not general "load N classes in between" noise.** Loading ~500 unrelated
  real JDK classes (`java.util.*`, `java.time.*`, `java.util.concurrent.*`,
  etc.) before running the 3-class sequence does not reproduce it. Loading
  exactly 11 *unrelated* classes (padding, chosen to match the class-count
  delta actually observed) in the same position also does **not** reproduce
  it — ruling out "class-table size/class_id-magnitude" as the trigger.
- **Not "any annotation lookup" or "any enum-valued annotation".** Replacing
  `getAnnotation(SupportedValidationTarget.class)` with
  `getAnnotation(Deprecated.class)` (a JDK-bootstrap-resident, always-loaded
  annotation) does **not** reproduce it. A self-defined, structurally
  identical annotation (`@interface MyAnno { MyEnum[] value(); }`, its own
  fresh enum, `@Retention(RUNTIME) @Target(TYPE)`) does **not** reproduce it
  either — ruling out "annotation with an enum-array member" as a general
  pattern. The trigger is specific to the real
  `jakarta.validation.constraintvalidation.SupportedValidationTarget` /
  `ValidationTarget` classes from `jakarta.validation-api-3.1.1.jar`, in this
  exact position relative to Hibernate Validator's own type-variable use.
- **Position-dependent, not just presence-dependent.** Calling
  `getAnnotation(SupportedValidationTarget.class)` **once, before** any of
  the three validator classes are processed (forcing
  `SupportedValidationTarget`/`ValidationTarget` to load first) does **not**
  reproduce the hang — all three then resolve correctly. It must happen
  *interleaved*, specifically after `ConstraintValidator`'s own type
  parameters have already been cached by an earlier `extractValidatedType`
  call and before they are consulted again by a later one.

## One real, applied, and kept fix (does not fully resolve the hang)

`native_class_get_type_parameters` (`native-builtins/src/lang_class.rs`)
built a **brand-new** synthetic `TypeVariable` object on every single call to
`Class.getTypeParameters()`, instead of returning the same cached instance
HotSpot's `Class.getGenericInfo()` soft-reference guarantees across repeated
calls. Fixed to check `cached_building_type_parameter` first and only build +
cache on a genuine first request (mirrors the identity-stability contract
already documented — but not fully implemented — in this file's own
comments). This is independently correct (verified via `Main7`/`Main8`/`Main9`
reflection-based repros, all still pass with the fix), and moved the earliest
observed hang point later in the built-in constraint list (from
`AbstractInstantBasedTimeValidator` to `AbstractDecimalMinValidator`) —
concrete evidence this was a real, if partial, contributing bug. **It does
not fix the `SupportedValidationTarget`-triggered case above**, confirmed
still hanging with this fix in place (`Main10`, and the real
`RemappedErrorViewIntegrationTests` target both still hang identically).

An additional candidate fix was tried and **reverted** after disproving it:
`is_inherited_annotation` (`native-builtins/src/lang_class.rs`) called
`ctx.ensure_class_initialized(class_name)` instead of a non-initializing
`ctx.load_class(class_name)` — a real anti-pattern (matches the
already-documented `type_sig_to_java`/Joda `DateTimeZone.<clinit>` fix
elsewhere in the same file), but changing it did **not** avoid the
`Main10` hang, because `SupportedValidationTarget`'s class is already loaded
(has a `class_id`) by the time `is_inherited_annotation` runs — the
`ensure_class_initialized` branch is never taken in this repro. Not
reapplied, to avoid landing an unverified change; worth revisiting if the
real trigger point turns out to be somewhere else in the annotation-lookup
path.

## What's still needed

- The actual mechanism by which
  `Class.getAnnotation(SupportedValidationTarget.class)` — specifically,
  and specifically interleaved between two `TypeHelper.extractValidatedType`
  calls — corrupts whatever downstream state produces the `HashMap` cycle.
  Every angle tried so far (hash stability, equals correctness, cached
  `TypeVariable` object content, GC/root-remap correctness) checked out
  clean; the corruption must be somewhere not yet instrumented — candidates
  not yet ruled out: `HashMap`'s own internal bucket-table integrity for
  *this specific* map instance (not yet traced at the `putVal`/bucket level);
  an interpreter dispatch-cache poisoning effect on the `containsKey`/`get`
  callsite itself (see `reference_invoke_virtual_native_dispatch_cache_quirk`
  in project memory for a related, though not identical, precedent — that
  quirk is about `ctx.invoke_virtual` native-to-Java callbacks specifically,
  and this loop is driven by ordinary bytecode `invokeinterface`, so it does
  not directly apply, but the general "call-history-dependent VM behavior"
  class of bug is the closest known precedent); something specific to
  `jakarta.validation-api-3.1.1.jar`'s own class-file layout for
  `SupportedValidationTarget`/`ValidationTarget` (e.g. a multi-release jar
  entry, a module-info interaction, or a BootstrapMethods/constant-pool
  quirk) that a from-scratch class-file parser could plausibly mishandle
  in a way a simple hand-authored test class never exercises.
- No debugger is available on this box (confirmed in prior sessions), so
  further isolation needs either more `CRATONVM_DBG_*`-gated tracing (a
  `HashMap.put`/`resize`-level trace would need hooking the constant-pool
  interpretation for `java/util/HashMap`, not attempted this session) or a
  from-scratch reimplementation of `SupportedValidationTarget`'s *exact*
  classfile bytes (via `javap -v`/hexdump diffing against a trivially
  different one) to find what specifically differs from the
  always-safe `Main14`-style hand-rolled equivalent.
- A full-suite verification pass (`RemappedErrorViewIntegrationTests`,
  `BasicErrorControllerIntegrationTests`, `WebMvcAutoConfigurationTests`) once
  a real fix lands — the doc's original validation numbers
  (`RemappedErrorViewIntegrationTests` 2/2 @ 25.4s,
  `BasicErrorControllerIntegrationTests` 26/26 @ 528.5s) are the target to
  reproduce.

## Affected classes (confirmed)

| Module | Class |
|---|---|
| `module/spring-boot-webmvc` | `org.springframework.boot.webmvc.autoconfigure.error.RemappedErrorViewIntegrationTests` |
| `module/spring-boot-webmvc` | `org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerIntegrationTests` |

Given the actual root cause is inside `ConstraintHelper.forAllBuiltinConstraints()`
(Hibernate Validator's own built-in-constraint bootstrap, called once per
process the first time any bean needs Bean Validation), this almost
certainly affects **every** Spring Boot test that boots a context with
Hibernate Validator on the classpath and Bean Validation actually engaged
(which is most of them) — not specifically ones with an embedded web server
or a self-connect step. `WebMvcAutoConfigurationTests` (previously noted as
"just needs a longer timeout, not a deadlock") should be re-examined: it may
be hitting this same livelock rather than genuine cumulative per-boot
overhead, now that a *hang-without-timeout* mechanism unrelated to boot
count is confirmed to exist in the same build.
