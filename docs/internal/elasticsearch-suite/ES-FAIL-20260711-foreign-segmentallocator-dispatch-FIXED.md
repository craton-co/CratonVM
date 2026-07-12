# ES failure family - foreign SegmentAllocator allocate dispatch - FIXED

Status: FIXED

Date observed: 2026-07-11

Date fixed: 2026-07-12

## Current-dev evidence

Focused probe against current `dev` commit
`d274d898c43a4ca07ac877ba85543d153d2ea83c`, built as
`cratonvm-es-focused-currentdev-20260711-172542`:

| VM mode | Class result |
| --- | --- |
| HotSpot | PASS, 17 tests, 0 failures |
| CratonVM JIT on | FAIL, 0 tests, 2 bootstrap failures |
| CratonVM JIT off | FAIL, 0 tests, 2 bootstrap failures |

Representative class: `org.elasticsearch.core.FastMathTests` (`others`
index 13 in the compiled Elasticsearch fixture).

The first divergent exception is:

```text
java.lang.AbstractMethodError: method
java/lang/foreign/SegmentAllocator.allocate(JJ)Ljava/lang/foreign/MemorySegment;
has no Code attribute
```

Its stack begins at `SegmentAllocator.java:318`, called while Elasticsearch
initializes `JdkPosixCLibrary`, then `NativeAccessHolder` and
`BootstrapForTesting`. The bootstrap continues with native access disabled,
and the class later fails with downstream `NoClassDefFoundError`s. Those
downstream errors are consequences, not independent missing-class bugs.

## Diagnosis

`SegmentAllocator.allocate(long, long)` is an interface dispatch point in
the real JDK foreign-memory API. CratonVM is selecting the no-Code interface
declaration instead of the concrete allocator implementation, then attempts
to execute it. The failure is independent of the JIT and is distinct from
the already-fixed `MemoryLayout.varHandle` / `SegmentVarHandle` work.

## Repro

```text
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 \
  -Category others -Start 13 -Count 1 -Vm craton -Jit on -TimeoutSec 120 \
  -ElasticsearchRoot <compiled-elasticsearch> -RefCsv <compiled-elasticsearch>/cratonvm-suite/results.jit.all.tsv \
  -Exe <cratonvm-es-focused-currentdev-20260711-172542> -JdkHome /usr/lib/jvm/java-21-openjdk-amd64
```

Run again with `-Jit off`; the same abstract-method error must occur. The
HotSpot control passes with the same compiled fixture.

## Next step

Audit real-JDK interface method resolution for `SegmentAllocator` and add a
minimal `Arena` / `SegmentAllocator.allocate(long, long)` regression probe.
The fix must dispatch to the concrete allocator implementation rather than
providing a synthetic result for the abstract interface declaration.

## Resolution (2026-07-12)

### Root cause

The receiver at the crash site is CratonVM's own synthetic `Arena` object
(`Arena.ofAuto()`/`ofConfined()`/`ofShared()`/`global()`,
`native-builtins/src/panama.rs::register_pe_arena`), which already has a
correctly-registered native for `allocate(JJ)Ljava/lang/foreign/
MemorySegment;` directly on the class name `java/lang/foreign/Arena`. That
native was never reached because `Arena` is itself an interface (JDK
`java.lang.foreign.Arena extends SegmentAllocator`), and CratonVM's
receiver-based dispatch retargeting only recognises **concrete**
(`!c.is_interface()`) receiver classes as valid retarget targets -- a real
invariant for ordinary Java objects (a runtime class is never literally an
interface), but one CratonVM's own interface-stamped synthetic factories
violate.

Concretely, `Elasticsearch.initializeNatives` -> `NativeAccessHolder.
<clinit>` -> `JdkPosixCLibrary.<clinit>` calls `Arena.ofAuto().allocate
(MemoryLayout)`. `SegmentAllocator.allocate(MemoryLayout)` is a **default
method with real JDK bytecode** (confirmed via `javap` against the real
JDK 21 module image: `jimage extract` + disassemble
`java.lang.foreign.SegmentAllocator.class`) that internally does
`this.allocate(byteSize, byteAlignment)` -- an `invokeinterface` against
the **abstract** `SegmentAllocator.allocate(long, long)` declaration
(`allocate(long)`, the other default method the doc's original repro also
exercises, has the identical shape).

In `vm/src/vm/vm_exec.rs::invoke_on_class_shared_inner`:

1. The "C25" interface-retarget block (top of the function) tries to
   re-point `class_id` from the constant-pool-resolved `SegmentAllocator`
   onto the receiver's actual runtime class, but only when
   `recv_is_concrete` (`!c.is_interface()`) holds for that receiver class.
   Since the receiver's runtime class is `java/lang/foreign/Arena` --
   itself an interface -- this check is false, so `class_id` never leaves
   `SegmentAllocator`.
2. `find_method_recursive(class_id=SegmentAllocator, "allocate", "(JJ)...")`
   correctly finds `SegmentAllocator`'s own abstract declaration (no Code).
3. The existing "receiver's own-class native rescue"
   (`if !native && method.is_abstract() && class_id != declaring_id`) is
   gated on `class_id != declaring_id` -- but since step 1 never retargeted
   `class_id`, both are `SegmentAllocator`, so this rescue never even looks
   at `Arena`'s native registry.
4. Dispatch falls through to `AbstractMethodError`.

This is the same general "interface dispatch resolves to the abstract
declaration instead of the concrete implementation" bug family described in
`docs/internal/comparison-handoff/bug-interface-method-dispatch-no-code-
attribute.md`, but a different specific cause than either of that doc's two
manifestations (`Collector.accumulator()`, `ServiceLoader$Provider.type()`)
and different from the FloatBuffer family
(`docs/internal/elasticsearch-suite/
ES-FAIL-FAMILY-20260710-floatbuffer-abstract-receiver-nocode-FIXED.md`,
where the receiver was allocated under an **abstract class** name, not an
**interface** name -- abstract classes already pass the `!c.is_interface()`
check, so that family's fix was a native-registration gap, not a dispatch
gap). This is the first confirmed occurrence where CratonVM's own
interface-stamped synthetic receiver (not a real-bytecode class) breaks the
`recv_is_concrete` invariant the retarget/rescue logic depends on.

### Fix

`vm/src/vm/vm_exec.rs::invoke_on_class_shared_inner` -- added a second,
narrower rescue immediately after the existing `class_id != declaring_id`
one: when a resolved method is still abstract and native, recompute the
receiver's *actual* runtime class directly from `args[0]` (independent of
whether the interface-exclusion above retargeted `class_id`) and check
*that* class's native registry too. This generalises the existing
"receiver's own-class native rescue" pattern to interface-stamped synthetic
receivers, not just concrete ones, without touching the broader retarget
semantics used by many other already-tuned dispatch special cases in the
same function.

No new native registration was needed -- `Arena.allocate(JJ)` was already
correctly registered; the bug was purely in *finding* it.

### Regression test

`vm/tests/es_segalloc_arena_dispatch.rs` -- compiles a minimal
`Arena`/`SegmentAllocator` probe (both `allocate(long)` and
`allocate(MemoryLayout)` default-method shapes, matching the two call
sites Elasticsearch's own `JdkPosixCLibrary` uses) against a real JDK 21
image and runs it through the `cratonvm` CLI under both `--nojit` and JIT
modes. Confirmed the test fails with the exact bug signature
(`AbstractMethodError: ... SegmentAllocator.allocate(JJ)... has no Code
attribute`) against an unpatched binary and passes cleanly against the fix.

Compiling `java.lang.foreign.*` (a JDK 21 preview API) needed two
environment workarounds on the Linux validation host, both implemented in
the test's `compile_probe`/`strip_preview_minor_version` helpers so the
test degrades gracefully elsewhere:
- The host's `java-21-openjdk-amd64` install is JRE-headless (no standalone
  `javac` binary). Falls back to invoking the bundled `jdk.compiler` module
  directly: `java --module jdk.compiler/com.sun.tools.javac.Main -source 21
  --enable-preview`.
- `--release 21` fails on that image (`lib/ct.sym` cross-release symbol
  data is not present in a JRE-headless install); `-source 21` avoids that
  need entirely.
- CratonVM's `ClassFileVersion::is_supported` (`reader/src/
  class_file_version.rs`) only accepts the preview minor-version marker
  (`0xFFFF`) when major equals its own `MAX_SUPPORTED` (currently Java 25),
  so a JDK-21-preview-compiled class file (major 65, minor `0xFFFF`) is
  rejected outright even though the bytecode itself is valid and already
  supported. The real Elasticsearch fixture's own class files sidestep this
  because ES's Gradle toolchain compiles with a newer JDK against which
  `java.lang.foreign` is no longer preview-annotated, producing a plain
  (minor 0) class file targeting `--release 21`. The test mirrors that
  shape by zeroing the compiled probe's minor-version bytes post-compile.

### Verification

Worktree: `/data/repo-es-segallocdispatch-20260712-174958` (Azure host
`20.83.144.174`; a plain `git clone` off the shared `/data/data/cratonvm`
checkout rather than a `git worktree`, because `/data/data` had 0 bytes
free at validation time -- see "environment notes" below).

Branch: `codex/es-segallocdispatch-20260712-174020`.

Binary: `/data/cratonvm-targets-alt/es-segallocdispatch-20260712-174958/
release/cratonvm-es-segallocdispatch-20260712-174958`.

Baseline (unpatched, for A/B comparison): a sibling clone at
`/data/repo-es-segallocdispatch-baseline-20260712-174958`, checked out at
unmodified `origin/dev` (`4e8b21ea`), binary
`/data/cratonvm-targets-alt/es-segallocdispatch-baseline-20260712-174958/
release/cratonvm-es-segallocdispatch-baseline-20260712-174958`.

Commit merged into `origin/dev`: `78da07db1d6679cb203c76cc0919587d4f0a6284`
(merge commit `da3f6a7b19f5eb352adccce85970923db34dff8d`), confirmed an
ancestor of `origin/dev` HEAD after push via `git merge-base
--is-ancestor`.

**1. `CRATONVM_DBG_NOCODE=1` zero-hit confirmation (the core fix claim):**
`org.elasticsearch.core.FastMathTests` (category `others`, index 13),
`-Vm craton`, both `-Jit on` and `-Jit off`: zero `SegmentAllocator`/`Arena`
lines in `CRATONVM_DBG_NOCODE=1` output (was the doc's originally-reported
"first divergent exception" every time, both JIT modes). The bootstrap now
progresses past `NativeAccessHolder.<clinit>` entirely, hits the fixture's
own pre-existing, unrelated `libvec.so`-missing `UnsatisfiedLinkError`
(gracefully caught, logs a `WARN`, continues with native access disabled --
**HotSpot hits this identical warning against the same fixture and still
passes 17/0**, confirmed live), then fails downstream on
`NoClassDefFoundError: com/fasterxml/jackson/core/util/
JsonRecyclerPools$ThreadLocalPool` / `org/elasticsearch/xcontent/
XContentType`. That is a separate, already-documented, pre-existing
CratonVM gap (ES's `x-content` module loads its bundled `jackson-
core-2.17.2.jar` as a **nested jar-within-a-jar** under
`IMPL-JARS/x-content/jackson-core-2.17.2.jar` via ES's own
`EmbeddedImplClassLoader`; see `docs/internal/
ES-xcontent-jackson-streamreadconstraints-loader-blind-invokestatic-
FIXED.md` and `docs/internal/elasticsearch-xcontent-provider-module-
descriptor-null.md` for the established history of this bug family), not
part of this fix's scope -- see "Known residual" below.

**2. Direct minimal-probe confirmation (isolates the fix from the Jackson
residual entirely):** a hand-compiled `Arena`/`SegmentAllocator` probe
(the same shape as the regression test, run directly against both
binaries, `--java-home /usr/lib/jvm/java-21-openjdk-amd64`):
  - Baseline (unpatched): `--nojit` throws exactly
    `AbstractMethodError: method java/lang/foreign/SegmentAllocator.
    allocate(JJ)Ljava/lang/foreign/MemorySegment; has no Code attribute`
    at `Probe.java:14` / `SegmentAllocator.java:318` -- reproducing the
    doc's exact signature.
  - Patched: both `--nojit` and JIT-on print all three expected lines
    (`confined.allocate(long).byteSize=64`,
    `auto.allocate(MemoryLayout).byteSize=4`,
    `shared.allocate(MemoryLayout).byteSize=8`) and `OK`, no exception.

**3. Broader-slice generalisation (`-Category others -Start 1 -Count 60`,
both JIT on/off):** zero `SegmentAllocator`/`Arena` no-Code hits across all
60 classes (was universal pre-fix per the doc's own "every ES test that
goes through `BootstrapForTesting`" claim). Two classes newly PASS
(`org.elasticsearch.cli.terminal.internal.EcsJsonUtilsTests`,
`org.elasticsearch.client.RestClientGzipCompressionTests`); the remainder
still FAIL/HANG on the separate Jackson/nested-jar gap above, but every
class now fails fast (~7-8s) with a clear, comprehensible error instead of
the reference baseline's `rc=124`/`HANG` (`results.jit.all.tsv` predates
this specific `AbstractMethodError`-throwing behaviour and recorded these
classes as outright hangs).

**4. Regression check (`-Category passed -Start 1 -Count 52`, all
previously-fully-passing classes, JIT on):** an initial `-Parallel 4` run
showed 10 classes flipping PASS->FAIL vs. the reference baseline. Direct
investigation of all 10 found every one failing with the *identical*
signature -- `RuntimeException: Failed to get a temporary name too many
times, check your temp directory ... /tmp` -- a host-level `/tmp`-exhaustion
symptom (this Azure host runs ~15-20 concurrent unrelated sessions),
**not** the Jackson error or any SegmentAllocator/Arena signature. Re-ran
the same 52-class slice at `-Parallel 1` (serial, avoiding the multi-JVM
`/tmp` contention `-Parallel 4` introduces on top of the shared host's
existing load): all 10 passed cleanly, and the **full 52-class PASS/FAIL/
HANG status set matched the unpatched-baseline clone byte-for-byte**
(24 PASS / 25 FAIL / 3 HANG in both). Confirmed zero genuine regressions.

**5. `cargo test -p cratonvm-vm --lib`:** 2196 passed, 11 failed, 111
ignored on the patched worktree -- and the **exact same 11 failing test
names** (all pre-existing, unrelated: `jit::skip_list::tests::*`,
`runtime::interpreter::tests::hot_files_have_no_production_panics`
(a ratchet-limit trip on `jit/src/x64.rs`, unrelated file), `vm::vm_init::
tests::*`) on a clean unpatched `origin/dev` clone built and run the same
way. Zero new failures from this fix.

**6. `vm/tests/es_segalloc_arena_dispatch.rs`:** passes against the patched
binary (both JIT-on and `--nojit` legs), fails with the exact
`AbstractMethodError` signature against the unpatched baseline binary --
confirmed the test is a real, non-vacuous regression guard.

### Known residual (separate bug, out of scope for this fix)

Getting past the `SegmentAllocator` dispatch crash exposes ES's
`XContentProvider`/`EmbeddedImplClassLoader` nested-jar (jar-within-a-jar)
classloading gap for essentially every ES test class that reaches
`BootstrapForTesting` (previously entirely masked by the earlier,
upstream `SegmentAllocator` crash). This is a distinct, already
partially-tracked bug family -- see `docs/internal/
ES-xcontent-jackson-streamreadconstraints-loader-blind-invokestatic-
FIXED.md` and `docs/internal/elasticsearch-xcontent-provider-module-
descriptor-null.md` -- not a new discovery, and not fixed here. It blocks
`FastMathTests` (and most of the `others` category) from reaching a full
17/0 PASS to match HotSpot; a follow-up investigation into
`EmbeddedImplClassLoader`'s handling of the `IMPL-JARS/<module>/
<jar-name>.jar` nested-jar convention is needed for that.
