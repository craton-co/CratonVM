# Two new Spring Framework FAILs found in the 2026-08-21 full-suite 3-GC rerun

## Status
**OPEN, both confirmed CratonVM-specific, GC-independent (fail on Generational/G1/ZGC alike), not yet root-caused.** Found 2026-08-21 re-running the
full 2,848-class Spring Framework suite on Azure (`20.80.105.49`) under all
three GC backends in parallel, on a fresh `dev`-tip build carrying the
2026-08-20 CHM/MethodHandle/JIT fixes. These are the only 2 of the 7
all-GC-common FAILs not already explained by an existing doc.

## 1. `VariableAndFunctionTests.functionViaMethodHandleForStaticMethodThatAcceptsOnlyVarargs()`

```
org.opentest4j.AssertionFailedError: [Did not get expected value for expression '#varargsFunctionHandle(null)'.]
expected: "[null]"
 but was: "null"
```

The test registers a `MethodHandle` for
`static String varargsFunction(String... strings) { return Arrays.toString(strings); }`
and evaluates a SpEL expression that invokes it with a single literal `null`
argument. Every other case in the same test — no args, an explicit array, 1-3
plain args, mixed types requiring conversion, args containing commas, and
`'a',null,'b'` (null as a NON-final vararg element) — all pass. Only the
single-bare-`null`-argument case fails.

**The values pinpoint the mechanism.** `expected: "[null]"` is
`Arrays.toString(new String[]{null})` — a 1-element array whose sole element
is null (the varargs collector wrapped the lone `null` argument into an
array, per Java's own vararg-collector semantics: an isolated `null` is
ambiguous between "this IS the array" and "this is one element," and the
collector is supposed to prefer the latter unless the argument is explicitly
cast/typed as the array type). `but was: "null"` is
`Arrays.toString((String[]) null)` — CratonVM's `MethodHandle` varargs
adapter instead passed the literal `null` through AS the array reference
itself, skipping the wrap-into-single-element-array step.

This is very plausibly in the same `MethodHandle`/`Lookup.find*` varargs-
collector family as the neighboring fix already on `dev`
("`Lookup.find*`/`unreflect*` did not mark a variable-arity target's handle
as a varargs collector" — from the 2026-08-20 fix commit `cfafa17d0`) — this
looks like the one edge case (a bare `null` as the sole vararg argument) that
fix didn't cover, rather than a fresh, unrelated bug. Not confirmed; flagged
as the first thing to check.

## 2. `WebClientUtilsTests.opaqueUriUnchanged()`

```
org.opentest4j.AssertionFailedError:
expected: "GET mailto:user@example.com?subject=hello"
 but was: "GET mailto:"
```

An *opaque* URI (`mailto:user@example.com?subject=hello` — no `//`
authority, per `java.net.URI`'s classification) passed through
`WebClient`/`UriBuilder` unmodified should keep its entire
scheme-specific-part (`user@example.com?subject=hello`) intact. CratonVM
drops everything after the scheme colon, leaving just `mailto:`. This looks
like a `java.net.URI` handling gap specific to opaque URIs (which store their
whole scheme-specific-part as one opaque string, distinct from hierarchical
URIs' authority/path/query/fragment split) — plausibly CratonVM's URI
construction/rebuilding path only round-trips the hierarchical-URI fields and
silently discards an opaque URI's scheme-specific-part. Not isolated to a
minimal `java.net.URI`-only repro yet.

## Next steps
* For (1): a minimal, Spring-free `MethodHandle`-varargs-collector probe
  passing a bare `null` as the sole argument (mirroring the pattern already
  used for the `asSpreader`/`collectArguments` bugs this session) would
  confirm or rule out the shared-root-cause hypothesis in minutes.
* For (2): a minimal `java.net.URI` probe — construct/round-trip an opaque
  URI (`new URI("mailto:user@example.com?subject=hello")`, or via
  `UriComponentsBuilder`/whatever `WebClient` actually uses internally) and
  compare `toString()`/`getSchemeSpecificPart()` against HotSpot.

## Repro
```bash
cd apps/spring-suite-runner
JDK25=/data/toolchain/jdk-25 SPRING=/data/cratonvm/apps/spring-framework \
CRATONVM_BIN=<cratonvm-bin> ./run-suite.sh run --category all \
  --only 'VariableAndFunctionTests|WebClientUtilsTests' --tag verify-20260821
```
