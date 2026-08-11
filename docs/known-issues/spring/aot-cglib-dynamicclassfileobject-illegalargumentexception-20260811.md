# Spring AOT + CGLIB proxy generation — `IllegalArgumentException: DynamicClassFileObject`, 5/5 identical signature, clean under HotSpot

**Status:** OPEN (2026-08-11). Found in the small residual (~10-12 classes
per GC variant) left over after the classpath-completeness fix that landed
in the 2026-08-11 `dev` merge cleared out 97% of Spring Framework's
previous failures (see `gc-corruption-guard-fixed-by-dev-merge-20260810.md`
and this session's round-2 rerun for that context — this doc covers one
specific, well-characterized piece of what's left, not the whole residual
set). Binary: `/data/cratonvm/target-zgc/release/cratonvm-spring-default-postmerge2`
(commit `4c4fb3902`), default (Generational) collector; reproduces
identically on G1 and ZGC in the same rerun (not yet independently
re-verified across all three variants' logs — check before assuming, this
doc only traced the default-variant occurrence in detail).

## The failure

All 5 non-passing test methods in
`org.springframework.context.aot.ApplicationContextAotGeneratorTests` fail
with the **identical exception type and target class**:

```
processAheadOfTimeWhenHasCglibProxyAndMixedAutowiring()  :: java.lang.RuntimeException: java.lang.IllegalArgumentException: org.springframework.core.test.tools.DynamicClassFileObject
processAheadOfTimeWhenHasCglibProxyWithArgumentsUseProxy() :: java.lang.RuntimeException: java.lang.IllegalArgumentException: org.springframework.core.test.tools.DynamicClassFileObject
processAheadOfTimeExposeUserClassForCglibProxy()          :: java.lang.RuntimeException: java.lang.IllegalArgumentException: org.springframework.core.test.tools.DynamicClassFileObject
processAheadOfTimeWhenHasCglibProxyUseProxy()             :: java.lang.RuntimeException: java.lang.IllegalArgumentException: org.springframework.core.test.tools.DynamicClassFileObject
processAheadOfTimeUsesCglibClassForFactoryMethod()        :: java.lang.RuntimeException: java.lang.IllegalArgumentException: org.springframework.core.test.tools.DynamicClassFileObject
```

Every one of the class's 5 failing methods has "Cglib" in its name and is
about AOT-processing a `BeanDefinition` that needs a CGLIB proxy. The other
35 methods in the same class (40 total) pass. `DynamicClassFileObject` is
Spring's own in-memory `JavaFileObject` implementation (under
`org.springframework.core.test.tools`, part of its dynamic-compilation test
support — the same package family flagged as a classpath-completeness gap
in `gc-variant-fullsuite-classpath-gap-and-fails-20260810.md`, though that
specific gap is now fixed; this is a different, live failure that only
shows up once the classpath is complete enough to reach it). An
`IllegalArgumentException` naming that class, wrapped in a
`RuntimeException`, with the exact same one-line message across all 5
methods, reads as one shared defect — something CratonVM does differently
when handed a `DynamicClassFileObject` instance during CGLIB proxy class
generation/registration, not five independent test bugs.

**No deeper stack trace was captured this session** — the harness's
`raw.log`/`failcauses.log` record only the one-line
`<method> :: <exception>` summary, not a full trace. That's the main gap
before this can be root-caused; see Recommended next steps.

## Confirmed CratonVM-specific

`ApplicationContextAotGeneratorTests` run clean under plain HotSpot (real
JDK 25, same classpath, via this harness's own `KRun` driver):

```
RESULT org.springframework.context.aot.ApplicationContextAotGeneratorTests found=40 succ=40 fail=0 skip=0 abort=0 status=OK
```

40/40, zero failures — all 5 of the CratonVM-failing methods pass on
HotSpot with nothing else changed. This is not a flaky test or a fixture
gap.

### Repro

```
cd apps/spring-suite-runner
export SPRING=/data/cratonvm/apps/spring-framework
CP="$(cat "$SPRING/spring-context/build/cratonvm-testcp.txt")"

# CratonVM (fails):
CRATONVM_BIN=/data/cratonvm/target-zgc/release/cratonvm-spring-default-postmerge2 \
  ./run-suite.sh run --category all \
  --only '(^|\t)org\.springframework\.context\.aot\.ApplicationContextAotGeneratorTests$' \
  --jit on --jdk real

# HotSpot control (passes 40/40):
java -cp ".:$CP" KRun org.springframework.context.aot.ApplicationContextAotGeneratorTests
```

## Recommended next steps

1. **Get the real stack trace first** — re-run just this class with full
   exception output captured (the harness currently truncates to one
   line; either raise its capture limit for a targeted rerun, or invoke
   `KRun` directly against the CratonVM binary and let the JVM's default
   uncaught-exception handler print the full chain to stderr, matching
   what the HotSpot control run implicitly has available).
2. Once the trace names the actual call site, the natural next questions:
   is `DynamicClassFileObject` (an in-memory `SimpleJavaFileObject`
   subclass with custom byte storage) hitting a native/reflection path
   CratonVM validates more strictly than HotSpot does (an
   `IllegalArgumentException` is a classic "type/argument didn't match
   what a native check expected" shape), or is this related to CGLIB's own
   dynamic bytecode generation interacting with CratonVM's classloading in
   a way the 5 passing non-Cglib methods in the same class don't exercise.
3. Check whether the other ~5-7 residual failures per variant (WebSocket/
   reactive integration timeouts, Mockito verification mismatches — see
   the round-2 rerun report for the full list) are independent or share
   any relationship with this one before triaging them separately; nothing
   here suggests they're related, but that wasn't checked directly.

## Related

- `gc-corruption-guard-fixed-by-dev-merge-20260810.md` (`h2/`, this
  session's connective doc) — the merge that fixed the classpath gap and
  exposed this residual by making the previously-97%-masked failure set
  small enough to actually read.
- `gc-variant-fullsuite-classpath-gap-and-fails-20260810.md` (this folder)
  — the now-fixed classpath-completeness bug; explicitly a different,
  already-resolved issue from this one, despite both involving the
  `org.springframework.core.test`/AOT test-support package family.
