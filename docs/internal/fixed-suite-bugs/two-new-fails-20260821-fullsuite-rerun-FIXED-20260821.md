# Two new Spring Framework FAILs found in the 2026-08-21 full-suite 3-GC rerun — FIXED 2026-08-21

## Status
**BOTH FIXED and verified on the real failing test classes.** Measured on Azure
host 2 (`20.80.105.49`), `apps/spring-suite-runner/one.sh`, same driver, same
classpath, the BINARY the only variable:

| class | baseline `ba36a7afa` | with the fix |
| --- | --- | --- |
| `VariableAndFunctionTests` | 16 found, 15 succ, **1 fail** | 16 found, **16 succ, 0 fail**, `status=OK` |
| `WebClientUtilsTests` | 13 found, 12 succ, **1 fail** | 13 found, **13 succ, 0 fail**, `status=OK` |

The baseline row is not quoted from the original sweep — it is a fresh run of
the *same* binary shape from this branch's merge base, so the two rows differ in
one thing only.

Both root causes turned out to be a rule the VM applies from RUNTIME values
where the JDK applies it from a STATIC one, and in both the fix was to restore
the static distinction the VM had thrown away.

---

## 1. `VariableAndFunctionTests.functionViaMethodHandleForStaticMethodThatAcceptsOnlyVarargs()`

```
expected: "[null]"      Arrays.toString(new String[] {null})
 but was: "null"        Arrays.toString((String[]) null)
```

### Root cause

Not the varargs-collector *marking* the 2026-08-20 `cfafa17d0` fix added — that
was already right (`isVarargsCollector()` answered `true`). The defect is one
step later, in **which handle door the call came through**.

A `null` sitting in a varargs collector's trailing array slot is genuinely
ambiguous: it can BE the array, or it can be one element the collector must
wrap. The JDK resolves it from the CALL SITE's static parameter type, in
`MethodHandleImpl$AsVarargsCollector.asType` — the "pass it straight through"
shortcut is taken **only** when the caller's trailing parameter type is
assignable to the collector's array type:

```java
mh.invokeExact((String[]) null)      // (String[])… -> assignable -> passthrough -> "null"
mh.invokeWithArguments(nullArg)      // genericMethodType(1) -> (Object)… -> NOT
                                     // assignable -> collect -> "[null]"
```

`invokeWithArguments` always adapts to `genericMethodType(n)`, whose trailing
parameter is `Object`, so on that door a collector **always** wraps.

CratonVM has no `AsVarargsCollector` wrapper. It derives varargs behaviour from
the runtime argument shape at dispatch, in `collect_trailing_varargs`
(`native-builtins/src/lang_invoke.rs`), and that function had one rule for every
door:

```rust
Some(Value::Object(None)) => return params.to_vec(),   // "a null IS the array"
```

A null carries no runtime type, so with the door erased there was nothing left
to decide on — and the answer it picked is the one `invokeExact` needs, which is
the door Spring does not use. SpEL's
`FunctionReference.executeFunctionViaMethodHandle` calls
`methodHandle.invokeWithArguments(functionArgs)`.

### The fix

`MH_GENERIC_SPREAD`, a one-shot thread-local armed by the three
`invokeWithArguments` registrations and **taken** (cleared) at the top of
`mh_dispatch`. Taken rather than scoped on purpose: only the OUTERMOST handle is
adapted to the generic type, so every recursive dispatch an adapter arm makes
below sees `false` — which is what HotSpot does for e.g.
`filterArguments(collector, 0, f).invokeWithArguments(null)`.

`mh_dispatch` combines it with the handle's own collector marking and passes one
`generic_collector: bool` to `collect_trailing_varargs`, which uses it for the
null slot only, and only when the array is the syntactically LAST parameter
(JRuby's `(…, IRubyObject[] args, Block)` shape, which that function locates by
searching for the array anywhere, is therefore untouched).

The marking, not the target's `ACC_VARARGS`, is what gates it — because that is
what the JDK keys on, and `probes/MhVarargsNullProbe.java` has the pair of
controls that proves it: D03 (`asFixedArity()` on a variable-arity target →
does NOT collect) and D04 (`asVarargsCollector()` on a fixed-arity one → DOES).

### Two residuals found while fixing it, both fixed here

* **The `MH_KIND_CONSTRUCTOR` arm of `mh_dispatch` never collected at all.** A
  variable-arity constructor is a varargs collector like any other method
  (`findConstructor(Holder.class, methodType(void.class, String[].class))
  .isVarargsCollector()` is `true` on every JDK), but that one arm went straight
  to `adapt_invoke_args`. `ctor.invokeWithArguments("a", "b")` reached
  `<init>([Ljava/lang/String;)V` with two flat arguments and built an EMPTY
  array — `H[]` against HotSpot's `H[a, b]` (probe row B18).

* **`asFixedArity()` and `asVarargsCollector()` mutated the RECEIVER.** Both
  flipped the marking on the handle they were called on and handed it back, so
  one library calling `h.asFixedArity()` silently changed how every other holder
  of `h` dispatched it — and silently reverted the fix above for that handle.
  That is not hypothetical for the reported test's shape: SpEL registers ONE
  long-lived `MethodHandle` and re-reads `isVarargsCollector()` on every
  evaluation. Now both return a slot-for-slot COPY (`mh_clone_handle`) carrying
  the requested marking, which is what every JDK does. Measured end to end as
  `probes/MhIdentityProbe.java` row I28.

  Width is the ownership test: a handle narrower than the synthetic layout is a
  real-JDK one, or `panama.rs`'s compact downcall layout whose slot 0 is a
  native address, and both keep the old in-place behaviour rather than have this
  file guess about slots it does not own.

---

## 2. `WebClientUtilsTests.opaqueUriUnchanged()`

```
expected: "GET mailto:user@example.com?subject=hello"
 but was: "GET mailto:"
```

### Root cause

An OPAQUE `java.net.URI` — absolute, with a scheme-specific part that does not
begin with `/` — has NO query. Its whole SSP is one undivided string in which
`?` is an ordinary character; only the fragment is split off it. (`URI.Parser
.parse` calls `parseHierarchical` only when the SSP begins with `/`.)

`WebClientUtils.getRequestDescription` leans on exactly that, with the comment
*"also handles Opaque URI, which has only schemeSpecificPart"*:

```java
if (uri.getRawUserInfo() == null && uri.getRawQuery() == null && uri.getRawFragment() == null) {
    return sb.append(uri).toString();
}
```

CratonVM answered `subject=hello` for `getRawQuery()`, so the method fell
through to the hierarchical rebuild — which for an opaque URI has no host and a
null path to append, leaving just the scheme and its colon.

`net_phase_e.rs`'s `uri_query_units` located the query as "text between the
first `?` and the first `#` after it" with no opacity test at all. The
`getQuery`/`getRawQuery` registrations still CARRY the comment claiming they
handle this (*"Opaque URIs keep a literal '?' inside the scheme-specific part,
so parse from `uri_split` rather than a raw `find('?')` fallback"*) — they lost
the property when they moved off `uri_split` onto the units splitter, and the
comment did not move with the behaviour.

### The fix

`uri_query_units` now asks `uri_select_raw_path_units` first and answers `None`
when the path is null. "The path is null" **is** the JDK's definition of an
opaque URI, so the two rules can never drift apart again — and the same fix
lands on `getQuery`, `getRawQuery`, `native_uri_init`'s field-correction pass
and `relativize`'s target query in one place.

### Two residuals found while fixing it, both fixed here

* **`URI.resolve` treated an opaque base as hierarchical.**
  `URI.create("mailto:a@b").resolve("c@d")` answered `mailto:c@d`; HotSpot
  answers `c@d`. `URI.resolve`'s literal first line is
  `if (child.isOpaque() || base.isOpaque()) return child;` — an opaque base has
  no path to merge against, so RFC 3986 §5 does not apply and nothing of the
  base survives. `uri_resolve_ref` now short-circuits on either side, returning
  the reference text verbatim (the JDK returns the argument OBJECT there, so it
  is not normalized either).

* **`isOpaque()` and `isAbsolute()` asked whether a colon appeared ANYWHERE.**
  A colon inside a relative reference's first path segment is an ordinary
  character, so `/redirect:account?q=1` read as an absolute opaque URI on both
  predicates. They now use `uri_scheme_colon` (the rule already written for
  Spring's redirect view names) and a new `uri_text_is_opaque` that delegates to
  the same null-path definition. Probe row H11.

---

## Regression measurement

A 360-class slice of the Spring Framework suite, run **twice per class,
ABBA-interleaved** (the arm order alternates class by class so a drifting host
load cannot favour one binary systematically), same driver, same classpath,
binary the only variable:

|  | OK | FAIL |
| --- | --- | --- |
| baseline `ba36a7afa` | 357 | 3 |
| with the fix | **359** | **1** |

**Exactly two rows differ, and both are the target tests.** The one class that
still FAILs — `WebClientIntegrationTests` — scores identically on both binaries
(found=170, succ=168, fail=1, skip=1), so it is untouched by this change.

The slice is not arbitrary: every class of `spring-expression` (the
MethodHandle-dense module, and the one that owns failure 1), plus every class in
the tree whose name carries a `Uri`/`Url`/`URI`/`URL`/`Resource`/`Path`
concern (failure 2's blast radius: `java.net.URI` sits under class loading,
`URL` handling and every Spring resource path) or a
`Script`/`Groovy`/`Kotlin`/`MethodHandle`/`Invoc`/`WebClient`/`Codec` one (the
remaining MethodHandle corners). Driver: `/data/tnf-spring-ab.sh` over
`apps/spring-suite-runner/one.sh`.

## What is still open

Nine measured rows across the three probes still diverge, in four groups, and
each is recorded with its mechanism and why it was left in
`docs/known-issues/spring/methodhandle-and-uri-divergences-20260821.md`. In
short: the inexact `invoke` door cannot see its call-site descriptor
(C03/C07/C09); CratonVM stays more forgiving than HotSpot about an
already-packed array and an over-long argument list (B06, I06); `asType` still
adapts the receiver in place (I13/I14); and two hierarchical `URI.resolve`
behaviours outside the opaque family are wrong (R13/R15).

## Files

```
native-builtins/src/lang_invoke.rs     MH_GENERIC_SPREAD, mh_clone_handle,
                                       mh_with_varargs_marking, the
                                       MH_KIND_CONSTRUCTOR collection, and
                                       collect_trailing_varargs' null rule
native-builtins/src/net_phase_e.rs     uri_query_units' opaque guard,
                                       uri_text_is_opaque, uri_resolve_ref's
                                       short-circuit, isOpaque/isAbsolute
probes/MhVarargsNullProbe.java         + .expected.txt   (51 rows)
probes/OpaqueUriProbe.java             + .expected.txt   (28 URIs x 22 accessors
                                                          + 16 resolve rows)
probes/MhIdentityProbe.java            + .expected.txt   (29 rows)
```

## Repro

```bash
cd apps/spring-suite-runner
JDK25=/data/toolchain/jdk-25 SPRING=/data/cratonvm/apps/spring-framework \
CRATONVM_BIN=<cratonvm-bin> ./one.sh \
  org.springframework.expression.spel.VariableAndFunctionTests
```

and, VM-free:

```bash
javac -d /tmp/cls probes/MhVarargsNullProbe.java probes/OpaqueUriProbe.java \
                  probes/MhIdentityProbe.java
java -cp /tmp/cls MhVarargsNullProbe            # the oracle
<cratonvm-bin> --java-home $JDK25 -cp /tmp/cls MhVarargsNullProbe 2>/dev/null
```

Diff on **stdout only** — CratonVM's tracing goes to stderr and `2>&1` puts it
in the diff.
