# Spring Boot core39 Cluster B — diagnostics, process, Base64, byte-array — FIXED

## Scope

Follow-up cluster from `../../../known-issues/springboot/spring-boot-core39-residual-clusters-20260723.md`, Cluster B:

- `org.springframework.boot.diagnostics.analyzer.NoSuchMethodFailureAnalyzerTests`
- `org.springframework.boot.info.ProcessInfoTests`
- `org.springframework.boot.io.Base64ProtocolResolverTests`
- `org.springframework.boot.json.AppendableByteArrayTests`

Worktree `C:\craton\CratonVM-springboot-core39-clusterB-20260723`, branch
`codex/fix-springboot-core39-clusterB-20260723`.

## Root causes and fixes

**`Base64ProtocolResolverTests`** — `java.util.Base64`'s decode error message
used a custom `"Invalid base64 char: X"` string instead of real JDK's
`"Illegal base64 character <hex>"`, which the test asserts on
(`native-builtins/src/lib.rs::b64_decode`).

**`AppendableByteArrayTests`** — the streaming `CharsetEncoder.encode(CharBuffer,
ByteBuffer, boolean)` native re-emitted the UTF-16 byte-order-mark on every
`encode()` call instead of once per encoder session, corrupting output
whenever a small destination buffer forced multiple `encode()` calls (Spring's
`AppendableByteArray` deliberately uses a tiny growable buffer)
(`native-builtins/src/charset.rs::native_encoder_encode`). Fixed by adding a
GC-stable identity-keyed side table (`bom_key_for`/`bom_already_written`/
`mark_bom_written`, mirroring the established `properties_sidetable`/
`jca::key_factory` pattern) tracking BOM-written state per encoder instance,
cleared by a new `CharsetEncoder.reset()` override
(`clear_bom_state`) — needed because `AppendableByteArrayTests.writeUsingCache`
reuses a single cached encoder instance across independent messages.

A second, unrelated bug surfaced by the same class after merging `origin/dev`:
a recently-landed Hibernate-optimizer AssertJ native
(`native_assertj_standard_comparison_are_equal`/`assertj_objects_equal`)
detected arrays via `class_name_of_id(...).starts_with('[')`, but CratonVM
does not register array objects under a normal `"[B"`-style class name in
`class_manager` — the name lookup silently returned `""`/`None`, so every
array comparison fell through to the generic `Object.equals` (reference
identity) branch, reporting content-identical arrays as unequal. Confirmed
pre-existing on unmodified `origin/dev` (independent of this cluster's other
changes) before fixing. Fixed by detecting arrays via the reliable
`heap_kind_of`/`heap_element_type_of` heap-header accessors instead
(`native-builtins/src/lib.rs::assertj_objects_equal`/`assertj_arrays_equal`).

**`ProcessInfoTests`** — `MemoryUsage.getInit()` was hardcoded to `0` for heap
and `-1` (JMM "unavailable" sentinel) for every non-heap field, both failing
`isPositive()` assertions (`native-builtins/src/jmx.rs::getMemoryUsage0`).
Added a real `initial_heap_bytes()` `NativeContext` accessor wired to
`VmConfig::initial_heap_size` (`native-api/src/registry.rs`,
`vm/src/vm/vm_exec.rs`) for heap `init`, and a `loaded_class_count()`-derived
non-heap estimate (grounded in an already-tracked real quantity, not
fabricated) for non-heap `init`/`used`/`committed` — `max` stays `-1`,
matching real JVMs' undefined non-heap aggregate max. Also added
`jdk.management.VirtualThreadSchedulerMXBean` support (the interface itself
was already loadable as real JDK; `ManagementFactory.getPlatformMXBean`'s
dispatch table simply never had an entry for it), reporting real
`available_processor_count()` for parallelism and honest `0`s for
pool-size/mounted/queued (CratonVM has no per-scheduler thread accounting).

**`NoSuchMethodFailureAnalyzerTests`** — two independent bugs exposed by
`@ClassPathOverrides`/`ModifiedClassPathClassLoader`:

1. `extract_pd_code_source_url` only handled a synthetic `CodeSource` layout.
   A real-JDK-constructed `CodeSource`'s `location` field is a `java.net.URL`
   object, not a `String`, so `read_string` silently failed and every class
   loaded via an isolated `URLClassLoader` reported a `null`
   `ProtectionDomain` (`native-builtins/src/classloader.rs`). Fixed by
   reconstructing the URL string from its own `protocol`/`file` fields
   (matching how real `URL.toString()` builds `protocol + ":" + file`) when
   the direct string read fails.
2. A genuine cross-loader virtual-dispatch bug in `execute_invoke_kind`'s
   exotic-case fallback: when its own `dispatch_override` divergence check
   found no divergence (because `get_loaded_class_id` happened to already
   match the receiver at that moment), it fell through to the loader-blind
   `invoke_shared`-by-name path — but `invoke_shared`'s own internal
   `load_class_concurrent` can independently resolve the *same* class-name
   string to a *different* `ClassId` than `get_loaded_class_id` just
   returned, when two same-named classes are loaded (an old override jar's
   class vs. the main classpath's). That let `mimeType.isMoreSpecific(null)`
   — a genuinely absent method on the receiver's own class — silently
   dispatch to the *other* same-named class's bytecode instead of raising
   `NoSuchMethodError`. Fixed by preferring the receiver's own
   already-resolved, already-initialized `class_id` whenever available
   (excluding the stale-pointer sentinel and lambda-proxy receivers, which
   have their own dedicated handling — routing a lambda's SAM method call
   through this branch regressed `ProcessInfoTests.memoryInfoIsAvailable`'s
   `allSatisfy(lambda)` with `NoSuchMethodError: <unknown class N>.accept(...)`
   during iteration on this fix, since lambda-proxy class ids aren't normal
   `class_manager` entries with a walkable method table)
   (`vm/src/runtime/interpreter.rs::execute_invoke_kind`).

   Also fixed `NoSuchMethodError`'s message to use dotted (source-form) class
   names matching real JDK convention, since Spring's analyzer embeds the raw
   exception message verbatim in its `FailureAnalysis` description and one
   assertion checks for the fully-qualified dotted form
   (`vm/src/runtime/exceptions.rs::linkage_throwable`).

## Verification

Fresh release binary (`cratonvm-springboot-clusterb-20260723.exe`), both JIT
and `--nojit`, `-Parallel 2 -TimeoutSec 300`, all 4 cluster classes + the 6
green controls from the parent doc:

| Class | JIT | `--nojit` |
| --- | --- | --- |
| `NoSuchMethodFailureAnalyzerTests` | PASS | PASS |
| `ProcessInfoTests` | PASS | PASS |
| `Base64ProtocolResolverTests` | PASS | PASS |
| `AppendableByteArrayTests` | PASS | PASS |
| `ApplicationPidFileWriterTests` | PASS | PASS |
| `BeanDefinitionLoaderTests` | PASS | PASS |
| `ConfigTreeConfigDataLocationResolverTests` | PASS | PASS |
| `JakartaApiValidationExceptionFailureAnalyzerTests` | PASS | PASS |
| `NoSnakeYamlPropertySourceLoaderTests` | PASS | PASS |
| `ConfigDataEnvironmentPostProcessorIntegrationTests` | PASS (268s isolated; borderline under `-Parallel 2` contention, not a regression) | FAIL (pre-existing, see below) |

`ConfigDataEnvironmentPostProcessorIntegrationTests`'s
`runWhenHasNonOptionalImportAndIgnoreNotFoundPropertyDoesNotThrowException`
fails under `--nojit` on unmodified `origin/dev` too (verified with a
fixes-stashed rebuild and again with a from-scratch `origin/dev` worktree
build) — a pre-existing residual unrelated to this cluster, out of scope
here.

Also re-verified: after merging `origin/dev` (26 commits ahead of this
branch's fork point) the resulting diff against `origin/dev` touches exactly
the 8 files this cluster's fix owns, nothing else — no silent content loss
from the merge.
