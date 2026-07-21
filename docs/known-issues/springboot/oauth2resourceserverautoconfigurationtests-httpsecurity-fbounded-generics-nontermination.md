# `OAuth2ResourceServerAutoConfigurationTests` HANG — `HttpSecurityConfiguration`
# top-level-customizer generic-type resolution never terminates

**Status: OPEN.** Root cause narrowed to a strong, source-confirmed hypothesis
(F-bounded generic type resolution over `HttpSecurityBuilder<H extends
HttpSecurityBuilder<H>>` failing to terminate/memoize), but not proven or
fixed this session. Found as an incidental discovery while closing out an
unrelated ObservationRegistry `@ConditionalOnMissingBean`/classpath-exclusions
residual doc (already fixed/closed) — this is a distinct bug found in
passing, unrelated to that one.

## Symptom

`module/spring-boot-security-oauth2-resource-server`'s
`org.springframework.boot.security.oauth2.server.resource.autoconfigure.OAuth2ResourceServerAutoConfigurationTests`
HANGs deterministically — confirmed on a **fresh** `dev` build (origin/dev
`65c6021f9`, worktree branch `fix/oauth2-resourceserver-autoconfig-hang-20260720`)
across three separate runs this session:

1. 50-class parallel batch run, 180s timeout — HANG (reported by the task that
   started this investigation).
2. Isolated single-class run, `-Parallel 1 -TimeoutSec 300` — HANG, killed by
   the harness at the full 300s external timeout. `OAuth2ResourceServerAutoConfigurationTests`'
   *reactive* sibling (`ReactiveOAuth2ResourceServerAutoConfigurationTests`),
   run alongside it under the same conditions, passed cleanly both times —
   this is not a host-load false-timeout.
3. Isolated single-class run with the VM's own diagnostic watchdog armed
   (`-CratonArgs '--stack-dump-on-timeout=45'`) — the watchdog's 45s deadline
   fired and it captured live stack dumps of every registered thread before
   its own `abort()` (the resulting `rc=-1073740791`/`0xC0000409` in
   `results.tsv` is that deliberate watchdog abort, **not** a spontaneous
   crash — see "Diagnostic notes" below).

### Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -Vm craton -Jit on -TimeoutSec 300 -Parallel 1 `
  -SpringBootRoot C:\craton\CratonVM\apps\spring-boot `
  -JdkHome "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot" `
  -ClassList <TSV: header "module`tclass", row "module/spring-boot-security-oauth2-resource-server`torg.springframework.boot.security.oauth2.server.resource.autoconfigure.OAuth2ResourceServerAutoConfigurationTests">
```

To get a live diagnostic stack dump instead of just a timeout kill, add
`-CratonArgs '--stack-dump-on-timeout=45'` (single `key=value` array element —
passing it as two separate array elements causes PowerShell to mis-bind the
second element to the script's own `-Category` parameter).

## What actually hangs

Two of the class's early tests (both "should fail" tests that throw during
Spring Boot's property-binding validation, before any bean is actually
instantiated —
`autoConfigurationUsingJwkSetUriShouldFailIfJwsAlgorithmIsUnknown` and
`shouldFailIfBothAuthoritiesExpressionsAndAuthoritiesClaimNameAreSet`, by
their distinctive WARN messages) complete normally. JUnit5's own test-ordering
is unspecified/deterministic-but-not-source-order, so exactly which test is
third varies; in the captured run it was
`autoConfigurationShouldConfigureAudienceAndIssuerJwtValidatorIfPropertyProvided`
(source line 522) — but the hang is not specific to that one method. Every
test in this class shares one `WebApplicationContextRunner` field
(`contextRunner`, line 101) that always includes
`TestConfig`/`JwtDecoderConfig`/etc., all annotated `@EnableWebSecurity`
(lines 940, 970, 981) — **any** test that reaches actual `SecurityFilterChain`
bean creation (i.e. doesn't fail out during property binding first) hits the
same hang, which is why the affected test varies by run while the hang itself
is deterministic per-class.

Captured via `--stack-dump-on-timeout=45`'s thread summary:

```
--- T19.H1 thread summary: 5 registered thread(s) ---
  tid=0 os_tid=11572 name="main" alive=true daemon=false blocked=false roots=381 state="<unset>" top=org/springframework/security/config/annotation/web/configuration/HttpSecurityConfiguration.lambda$applyTopLevelCustomizers$1@40 <- org/springframework/util/ReflectionUtils.doWithMethods@66 <- org/springframework/security/config/annotation/web/configuration/HttpSecurityConfiguration.applyTopLevelCustomizers@21
  tid=1 os_tid=28940 name="Common-Cleaner" alive=true daemon=true blocked=true roots=6 state="<unset>" top=jdk/internal/ref/CleanerImpl.run@45 <- jdk/internal/misc/InnocuousThread.run@20
  tid=2 os_tid=29140 name="MockWebServer TaskRunner" alive=false daemon=true blocked=true roots=1 state="<no-frame-trace>"
  tid=3 os_tid=5776 name="MockWebServer TaskRunner" alive=false daemon=true blocked=true roots=1 state="<no-frame-trace>"
  tid=4 os_tid=6096 name="MockWebServer TaskRunner" alive=true daemon=true blocked=true roots=35 state="<unset>" top=sun/nio/ch/NioSocketImpl.accept@170 <- java/net/ServerSocket.implAccept@26 <- java/net/ServerSocket.platformImplAccept@31
```

Only `main` matters — the three `MockWebServer TaskRunner` threads are
ordinary `accept()`-blocked background threads (expected/inert), and
`Common-Cleaner` is idle. **`main` is NOT flagged `blocked=true`** — it is
executing real Java bytecode the entire time, not parked on a lock or a
native I/O wait. Its 156-frame captured stack shows a recognizable, self-
similar ~15-frame cycle appearing **twice** (nested) in the same snapshot:

```
...HttpSecurity.httpSecurity()                                    [bean factory method]
  -> HttpSecurityConfiguration.applyTopLevelCustomizers()
    -> ReflectionUtils.doWithMethods(HttpSecurity.class, ...)
      -> HttpSecurityConfiguration.lambda$applyTopLevelCustomizers$1(...)
        -> DefaultListableBeanFactory$1.orderedStream()
          -> DefaultListableBeanFactory.beanNamesForStream/getBeanNamesForType/doGetBeanNamesForType
            -> AbstractBeanFactory.isTypeMatch()
              -> ResolvableType.isInstance/isAssignableFrom/isAssignableFrom
                -> ResolvableType$WildcardBounds.get()
                  -> ResolvableType.resolveType()/getType()
                    -> SerializableTypeWrapper$TypeProxyInvocationHandler.invoke()  [pc=0]
                      -> (recurses back into HttpSecurity.httpSecurity() bean creation)
```

i.e. resolving the generic type of the `httpSecurity` bean (to find which
`Customizer<HttpSecurity>`-typed beans apply to it) re-enters **creation of
the `httpSecurity` bean itself**. Confirmed via `javap` against the real
`spring-security-config-7.1.0-RC1.jar` /
`spring-core-7.0.7.jar` (not any CratonVM-substituted class):

```
public final class HttpSecurity
    extends AbstractConfiguredSecurityBuilder<DefaultSecurityFilterChain, HttpSecurity>
    implements SecurityBuilder<DefaultSecurityFilterChain>,
               HttpSecurityBuilder<HttpSecurity>

public interface HttpSecurityBuilder<H extends HttpSecurityBuilder<H>>
    extends SecurityBuilder<DefaultSecurityFilterChain>
```

`HttpSecurityBuilder<H extends HttpSecurityBuilder<H>>` is genuine
**F-bounded polymorphism** — `HttpSecurity`'s own generic supertype graph
contains a real self-reference (`H` is bounded by a type that mentions `H`).
Spring's `SerializableTypeWrapper` (used because JDK reflection's
`ParameterizedType`/`WildcardType`/`TypeVariable` implementations aren't
`Serializable`) is depended on to terminate such graphs via its static
`cache: ConcurrentReferenceHashMap<Type, Type>` (confirmed via `javap` on the
real `spring-core-7.0.7.jar` — `forTypeProvider`'s bytecode does
`providedType instanceof Serializable` short-circuit, then
`cache.get(providedType)`, before falling through to wrap-and-`cache.put`).
If either (a) the leaf-level short-circuit or (b) the cache lookup fails to
recognize a Type it has already resolved, walking this self-referential graph
has no other termination condition.

## Diagnostic notes (how "livelock vs. deadlock" was determined)

This is **neither** of the two shapes the assigning task asked to
distinguish between:

- **Not the previously-fixed `InterceptingExecutableInvoker` JIT
  speculative-collection-probe livelock** (see
  [`../../internal/springboot/junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster-FIXED.md`](../../internal/springboot/junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster-FIXED.md)):
  that bug's signature was a fixed-periodicity `gc::guard` out-of-bounds-field
  warning repeating against a **zero-field** JUnit5 internal object with
  **zero forward progress** and **zero JUnit/application-level output**. This
  hang shows no `gc::guard` warnings at all, and the captured thread *is*
  making real forward progress through real Spring bytecode.
- **Not a classic lock-order deadlock** (cf. the OPEN, structurally-similar
  `class_manager`/`vtable_manager` AB-BA lock-order deadlock referenced
  elsewhere in this repo's internal notes) — a true Rust-lock deadlock would
  show the thread genuinely parked (flat CPU time, `blocked=true` or stuck at
  one unchanging `pc` across repeated samples). Here `main` is flagged
  `blocked=false` by the VM's own bookkeeping, and running the diagnostic
  watchdog produced **~1,600 successive stack-dump snapshots over roughly 8
  seconds** (45s deadline through ~53s final abort) with the captured frame
  count fluctuating between 153–156 across snapshots — i.e. the thread kept
  hitting fresh interpreter safepoints and doing further (bytecode-visible)
  work between each snapshot, not sitting motionless at one frozen `pc`.

This is a third category: a **non-terminating algorithm** — real, repeated
forward execution through a recursive resolution routine that structurally
never reaches its base case (or never hits its intended memoization) for this
specific self-referential generic type shape. The `rc=-1073740791`
(`0xC0000409`, Windows `STATUS_STACK_BUFFER_OVERRUN`/fastfail) recorded for
run 3 is the **watchdog's own deliberate `process::abort()`** after
dumping — documented behavior of `--stack-dump-on-timeout`, not evidence of
an organic native stack-overflow fault. Whether the underlying recursion
would eventually overflow the real call stack on its own (given enough time
under the default 300s external harness timeout, which is what actually
terminated runs 1 and 2) was not directly observed, since the diagnostic run
was intentionally aborted early at 45s by design.

The repeated-dump behavior of the watchdog itself (~1,600 snapshots instead
of one) is a secondary, minor observation worth a follow-up look at
`--stack-dump-on-timeout`'s implementation — it's presumably retrying the
dump on every interpreter safepoint poll until the target thread reaches
whatever it considers a "safe" dump point, and never does within the
window before its own abort fires. Not investigated further this session;
does not block using the flag as a diagnostic (it produced fully actionable
data here), just noisier/slower than the "single dump" the user-guide
(`docs/book/src/user-guide/debugging.md`) describes.

## Root cause: not fixed this session

Strong hypothesis, not proven: CratonVM's reflection/generic-signature
metadata likely does not intern/cache `TypeVariable`/`ParameterizedType`/
`WildcardType` instances the way the real JDK does (real JDK's
`sun.reflect.generics.repository` machinery caches these by
`(GenericDeclaration, name)` or structurally-stable identity, so repeated
introspection of the *same* generic parameter yields `Type` instances whose
`equals()`/`hashCode()` — and often reference identity — are stable across
calls). If CratonVM instead synthesizes a fresh, only-structurally-equal
`Type` object graph on each independent reflective walk, and if that
structural `equals()`/`hashCode()` doesn't correctly handle a
self-referential (cyclic) generic-bound graph the way real JDK's
implementations do, then `SerializableTypeWrapper.cache.get(providedType)`
could permanently miss for what should be "the same" `Type`, defeating the
one mechanism (`ConcurrentReferenceHashMap`-based memoization) that's
supposed to make this F-bounded traversal terminate.

**Explicitly not confirmed**: this session did not instrument the actual
reflection/generic-signature code path (`classloading`/`vm` crates) to watch
where the `HttpSecurity`-generic walk fails to shortcut, nor did it build a
minimal standalone repro isolating `HttpSecurityBuilder`-shaped F-bounded
generics + `SerializableTypeWrapper` without a full Spring context (the
`CrhmRepro.java`-style probe pattern used for
[`repeatablecontainers-method-cache-classcastexception-FIXED.md`](../../internal/springboot/repeatablecontainers-method-cache-classcastexception-FIXED.md)
would be the natural next step, and that doc's own standalone
`ConcurrentReferenceHashMap.computeIfAbsent` stress probe found **zero**
issues as of 2026-07-20 on this same `dev` baseline — but `SerializableTypeWrapper`
uses plain `get`/`put`, not `computeIfAbsent`, and the failure mode
hypothesized here is about `Type.equals()`/`hashCode()` identity stability
under self-referential generics, not `ConcurrentReferenceHashMap`'s own
segment/reference-purge correctness, so that clean result does not rule this
out).

## Suggested next steps

1. Write a minimal standalone Java repro: an interface
   `Self<T extends Self<T>>` implemented by a class `Impl implements
   Self<Impl>`, reflectively probe its generic supertype/interface types
   repeatedly through `SerializableTypeWrapper`-equivalent logic (or just
   directly exercise `ResolvableType.forClass(Impl.class).getInterfaces()`
   recursively), and confirm it hangs/recourses pathologically on CratonVM
   without needing Spring Security or Spring Boot at all.
2. If confirmed, instrument (`eprintln!` or existing `CRATONVM_DBG_*`
   facilities) CratonVM's generic-signature parsing
   (`classloading`/`vm` crates — wherever `getGenericSuperclass`/
   `getGenericInterfaces`/`TypeVariable.getBounds` are implemented) to check
   whether repeated introspection of the same type parameter returns
   `equals()`-stable `Type` objects, and whether that equality/hash
   implementation actually terminates when comparing two cyclic
   `TypeVariable` graphs (real JDK avoids ever doing deep structural
   comparison of the cycle by using declaration-site reference/cache
   identity — if CratonVM's `TypeVariable.equals()` does deep structural
   comparison instead, that itself could non-terminate independent of any
   Spring-side cache).

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-security-oauth2-resource-server` | `org.springframework.boot.security.oauth2.server.resource.autoconfigure.OAuth2ResourceServerAutoConfigurationTests` |

Likely affects any test/app exercising Spring Security's
`HttpSecurityConfiguration`/`HttpSecurityBuilder` machinery broadly (i.e. any
`@EnableWebSecurity` context that reaches `HttpSecurity` bean creation with
customizer-type beans present), not just this one class — not verified
against other Spring Security test classes this session.
