# ES x-content Jackson `StreamReadConstraints.maxNameLength` NoSuchMethodError

Status: FIXED

## Context

Un-masked 2026-07-10 while verifying the fix for
`docs/internal/fixed-suite-bugs/ES-FAIL-FAMILY-20260710-build-currentholder-manifest-null-FIXED.md`
(a VM-core `jdk/internal/misc/Unsafe` bootstrap bug that previously crashed
every affected class before any real test logic ran). With that bug fixed,
`org.elasticsearch.cli.terminal.internal.EcsJsonUtilsTests` now reaches real
test execution and immediately hits this separate, pre-existing bug instead.

This is **not a new bug** — it was already independently investigated on
2026-07-03 (see prior session notes referencing branch
`investigate/es-jackson-invokestatic-loader-blind-20260703`, commit
`da66940c`), but that investigation's write-up never landed in
`docs/known-issues` or `docs/internal` on `dev` — only this fresh repro
against current `dev` confirms it is still present. Filing it here per the
known-issues triage rule (a residual must have its own live doc, not just be
asserted as "tracked elsewhere").

## Symptom

```
java.lang.NoSuchMethodError: com/fasterxml/jackson/core/StreamReadConstraints$Builder.maxNameLength(I)Lcom/fasterxml/jackson/core/StreamReadConstraints$Builder;
	at org.elasticsearch.xcontent.provider.XContentImplUtils.configure(...)
```

followed by (once the first failure poisons class state for the run):

```
java.lang.NoClassDefFoundError: org/elasticsearch/xcontent/XContentType
```

Reproduced via `org.junit.runner.JUnitCore` against
`org.elasticsearch.cli.terminal.internal.EcsJsonUtilsTests` with the
standard `run-elasticsearch-suite.ps1` flags (`--java-home`, JIT on,
seed `B17AC9D3E1F2A0C4`).

## Root cause (per the 2026-07-03 investigation, not re-verified in depth here)

ES's `x-content` module bundles its own newer `jackson-core-2.17.2.jar`
*inside* `elasticsearch-x-content-*.jar` (loaded via ES's own
`EmbeddedImplClassLoader`, unrelated to CratonVM). The outer application
classpath separately carries an older `jackson-core-2.15.0.jar` (missing
`maxNameLength`, added in Jackson 2.16+). `XContentImplUtils` (also defined
by the `EmbeddedImplClassLoader`) calls
`StreamReadConstraints.builder().maxNameLength(...)` — an `invokestatic` on
`.builder()`. The embedded jar genuinely has the newer class with the
method (`javap`-confirmed); `Class.forName` with explicit, distinct
loaders correctly produces two distinct `Class` objects with the right,
differing method sets. `CRATONVM_LOADER_AWARE_RESOLUTION=1` (the gate that
fixes the closely-related `CONSTANT_Class`/`new`/`checkcast`/`instanceof`/
field-owner loader-blind resolution family — see
`docs/internal/fixed-suite-bugs/hib-proxyclassreuse-loader-blind-class-resolution-FIXED.md`)
does **not** fix this case: that gate's `resolve_class_loader_aware`/
`lookup_loader_initiated` covers `ldc`/`new`/`checkcast`/`instanceof`/
`anewarray` and field resolution, but `invokestatic`'s *owner*-class
resolution is a separate code path (`execute_invokestatic`,
`vm/src/runtime/interpreter.rs`) not covered by that gate. The suspected
actual resolution-bug site is `StreamReadConstraints.builder()`'s
`invokestatic` itself resolving `StreamReadConstraints` to the wrong
(outer-classpath, older) version — the returned `Builder` instance is
genuinely a 2.15.0 one, hence the correctly-reported downstream
`NoSuchMethodError` on `.maxNameLength`.

## Suggested next step

`execute_invokestatic`'s owner-class resolution needs the same
loader-faithful treatment `resolve_class_loader_aware` already gives
`ldc`/`new`/`checkcast`/etc. — likely by threading the calling frame's
defining loader through to the owner-class lookup the same way
`vm/src/runtime/interpreter.rs::execute_invokestatic`'s existing
same-class-self-call special case (see the "Residual B" fix in
`hib-proxyclassreuse-loader-blind-class-resolution-FIXED.md`) already does
for the narrower self-call case. Scoped project, not a one-liner — same bar
as the rest of the loader-blind-resolution family (full app-gauntlet soak
before flipping any related default).

## Resolution

Fixed 2026-07-11. Global class loading no longer defines ES IMPL-JARS classes
in the flat application namespace. Context-free x-content provider loading now
uses Elasticsearchs EmbeddedImplClassLoader, and gated invokestatic owner
resolution drives the callers defining loader before accepting a same-named
global class.

For the outer Jackson 2.15 compatibility surface, CratonVM now implements the
newer fluent Builder.maxNameLength(int) entry point as a receiver-preserving
constraint hook. This lets the ES x-content provider configure its newer API
without poisoning class initialization when the older application Jackson is
also present.

Verification: a fresh JIT-on JUnitCore run of
org.elasticsearch.cli.terminal.internal.EcsJsonUtilsTests with seed
B17AC9D3E1F2A0C4 completed OK (14 tests) on the uniquely built remote binary.

## Impact

At minimum `EcsJsonUtilsTests`; likely other classes in the same
`libs/cli-terminal`/`x-content`-dependent slice of the ES suite family that
was previously masked entirely by the `Build$CurrentHolder` manifest-null
bug. Re-triage needed once a full ES-suite run against the fixed binary is
available (out of scope here — see the manifest-null fix doc's verification
section).
