# Spring's `java.lang.classfile` annotation metadata reading is corrupted under CratonVM — RESOLVED

**Status: FIXED (2026-08-05).** Same defect as
`flywayautoconfigurationtests-timeout-jit-site-cache-aliasing-FIXED-20260805.md`,
seen from the webmvc side. Read that page for the full root cause, the
bisection that identified it, and the cross-platform validation.

## Original symptom (as filed)

Three classes in the webmvc/web-server bootstrap area failed heavily, all
inside Spring's annotation-metadata reading machinery
(`org.springframework.core.type.classreading.*` /
`org.springframework.core.type.AnnotatedTypeMetadata` /
`org.springframework.core.annotation.MergedAnnotations`), with three
distinct-looking but same-family symptoms:

1. `WebMvcAutoConfigurationTests` — **88/93 tests fail** with
   `java.lang.classfile.constantpool.ConstantPoolException: Bad CP index: 23296`
   (a second bad index, `23041`, also appeared) out of
   `ClassFileAnnotationDelegate.createMergedAnnotations` →
   `ClassFileMethodMetadata.of` → `ClassFileMetadataReader.<init>`.
2. `WebMvcObservationAutoConfigurationTests` — **12/13 fail** with
   `NullPointerException: Cannot invoke "MergedAnnotation.isPresent()" because "annotation" is null`.
3. `ServletComponentScanIntegrationTests` — **2/3 fail** with
   `NullPointerException: … the return value of "AnnotatedTypeMetadata.getAnnotations()" is null`.

Plus, flagged as unconfirmed at filing time: `WebTestClientAutoConfigurationTests`
(3/11 fail, `IllegalStateException: No ConfigurableListableBeanFactory set`).

The filed hypothesis — that CratonVM served corrupt `.class` *bytes* to this
reader — was wrong. The bytes are fine; a probe that reads the identical
resource through `Files.newInputStream(...).readAllBytes()` and parses it with
`ClassFile.of().parse()` 192k times passes on the pre-fix binary.

## Root cause

`383e7f5cf` — *a recycled `JitInvokeInfo` address let one call site serve
another's dispatch*. Site-keyed per-thread dispatch memos (including
`NATIVE_SITE_CACHE`, which after `836631dcc` holds a resolved native callback
for **every** native call from compiled code) were not flushed on a JIT cache
generation change, so a `JitInvokeInfo` box whose address had been recycled
into a new compile inherited the previous site's resolution and called a
different native.

Spring's `ClassFileMetadataReader` path is native-call-dense and re-runs on
every `ApplicationContext` refresh, which is why these classes concentrated
the damage. `Bad CP index: 23296` is not a parser reading the wrong buffer —
it is an index-returning native answering from the wrong call site, which is
also why the indices were implausibly large.

## Validation

All four classes on the fixed binary (dev with `383e7f5cf`, local Windows,
one process per class, runner env vars set):

| Module | Class | Before | After |
|---|---|---|---|
| `module/spring-boot-webmvc` | `WebMvcAutoConfigurationTests` | 88/93 fail | **93/93 pass** |
| `module/spring-boot-webmvc` | `WebMvcObservationAutoConfigurationTests` | 12/13 fail | **13/13 pass** |
| `module/spring-boot-web-server` | `ServletComponentScanIntegrationTests` | 2/3 fail | **3/3 pass** |
| `module/spring-boot-webtestclient` | `WebTestClientAutoConfigurationTests` | 3/11 fail | **11/11 pass** |

That last row also settles the "possibly related, unconfirmed" note in the
original page: it was the same defect.

HotSpot baseline (`hotspot-baseline-latest.tsv`) passes all four, so these
were genuinely CratonVM-specific and not the CRLF-fixture confound.

**Note on `WebMvcAutoConfigurationTests` wall time:** it now passes, but took
455s on a heavily loaded local box. Its own budget behaviour against the
suite's 300s ceiling is a separate throughput question from this correctness
bug and is not tracked here — check it against the HotSpot baseline row before
filing anything new.

## Regression guard

`a_jit_generation_change_clears_every_site_keyed_memo` in
`vm/src/jit/helpers.rs` (shipped with `383e7f5cf`): populates all eight
site-keyed memos, forces the generation to differ, asserts each is emptied,
and asserts non-emptiness first so it cannot pass on maps that were already
clear.
