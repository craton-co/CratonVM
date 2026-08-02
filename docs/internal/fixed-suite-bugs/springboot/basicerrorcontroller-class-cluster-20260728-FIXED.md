# `BasicErrorControllerIntegrationTests` comparator crash and residuals

**Status: FIXED — 2026-08-01.** Reported 2026-07-28, fixed 2026-07-29,
regressed 2026-07-31, and closed here. This file is the record for the whole
cluster: it absorbs the two companion reports filed on the same class,
`springboot-basicerrorcontroller-checkcast-abort-20260731.md` (the GC root
cause) and `basicerrorcontroller-jit-only-failure-20260731.md` (the `<local5>`
arm, and why this class was not a usable acceptance gate). Both are retired
alongside it, bodies intact, in this directory.

Two independent defects were behind the five affected classes, and **neither is
the one the original 2026-07-28 report named**:

1. a GC one — collection-overlay backing stores reclaimed while their owner was
   live — fixed across `3211b8c74`, `c3dbb011a`, `0b18f15eb` and `19cb55343`;
2. a JIT one — the JIT-to-JIT exception-handler resume rebuilding a handler
   frame from the callee's arguments alone. Fixed twice, independently and on
   the same day: `843b780baa` on this branch, reached from
   `BasicErrorControllerDirectMockMvcTests`, and `063be4f186` on `dev`, reached
   from `LiquibaseAutoConfigurationTests`. Both landed on the identical
   `BindConverter.convert` witness. `dev`'s version is the one that survives
   the merge — it is a superset — and this branch keeps only the piece dev did
   not have: the same fail-closed refusal in the OTHER sink,
   `route_jit_signal_exception`.

A third, unrelated defect turned up while validating and is fixed here too
(`6c2a8a677d`): `Locale.toString()` returned `""` for every real-JDK `Locale`.

## What the reports said, and what was actually true

The 2026-07-28 report terminated the real HTTP-client integration path with:

```
NoSuchMethodError
java/lang/String$CaseInsensitiveComparator.apply(Ljava/lang/Object;)Ljava/lang/Object;
caller=org/springframework/http/client/JdkClientHttpRequest.lambda$buildRequest$0
internal error: checkcast: not an object reference
```

`String.CASE_INSENSITIVE_ORDER` is a `Comparator`, not a `Function`, so the
`apply(Object)` receiver shape read as a JIT dispatch corruption and the first
round of fixes was four JIT-admission bans plus a native-collector repair. That
diagnosis was wrong twice over, and both corrections are already on the record:

* The `apply(Object)Object` warning is `comparator_compare`'s documented
  key-extractor fallback. It runs only **after** the real `compare` has already
  failed, so it is a symptom, never a cause. A pristine-dev control with every
  ban in place reproduced the abort at the same rate.
* The real fault was a GC one — a `TreeSet`/`TreeMap` collection-overlay
  backing array reclaimed while its owner was still live, so the comparator was
  handed `Int(0)` off a freed, zeroed array. See "The first defect" below.

The second report was likewise misread. Its
`ClassCastException: java.lang.Object cannot be cast to ...ConditionAndOutcomes`
was attributed to `ConditionEvaluationReport.lambda$recordConditionEvaluation$0`
producing a raw `Object` once the `SPRINGBOOT-CONDITION-REPORT.1` ban was
lifted. `java.lang.Object` in a cast message is not a lambda's return value —
it is what a **reclaimed or stale `ObjectRef` prints**, because a freed object
reads back as `ClassId(0)`, and the codebase already carries that reading in two
places (`jit/helpers.rs`'s `FMT-JIT-CCE` note and `vm_exec.rs`'s `cceres3` note,
both naming `Object cannot be cast to X` as the poisoned-island signature). So
the report's cast failures belong with the GC family below, not with the
ban removal.

**Stated plainly, because it matters for anyone reopening this:** that CCE was
NOT re-observed this session. It did not appear once across 12 runs of
`BasicErrorControllerIntegrationTests`, 7 of `BasicErrorControllerDirectMockMvcTests`,
5 of `OAuth2ResourceServerAutoConfigurationTests` and 5 of
`CloudFoundryActuatorAutoConfigurationTests`, so it was never root-caused
directly — the attribution above is from its signature and from the GC work
that landed in between, not from a live repro. What DID reproduce on those
classes, deterministically, was a different failure the reports also recorded
(the `<local5>` arm), and that one is a JIT bug with nothing to do with GC. Both
are covered below.

## The first defect — reclaimed collection overlays (GC)

Root cause and fix are `3211b8c74`, "root collection-overlay refs in the moving
young collector", written up in the companion report:

1. `native_roots::scan_collection_overlays` skips the unconditional overlay root
   scan on the Generational collector under several conditions, relying instead
   on the marker walking each owner. `sweep_young_non_moving` and `old_gen_gc`
   implement that walk; the **moving Cheney young path never did**. Once
   moving-young became the default, every overlay-held young array was
   silently reclaimable. Fixed by seeding the Cheney evacuation from
   `external_roots_for_matching_owners(&|_| true)`.
2. Ten TreeSet/TreeMap sites published a freshly allocated backing array through
   the **pre-allocation** `this`, registering the overlay under a stale owner
   address. Fixed by `ts_install_backing_array` / `tm_install_backing_array`,
   which pin both the owner and the array across the allocation and the store.

That fix validated 20/20 clean and then appeared to regress the same day, in
the 49-class residual rerun and again on `JettyServletWebServerFactoryTests`.
It did not regress: the fix was real but incomplete, and the remaining holes
were closed over 2026-07-31/08-01 by the old-gen and side-table work that
landed after the binaries those reruns used —

* `c3dbb011a` the old-gen mark must not accept unvalidated addresses, and
  old-gen liveness must be free-list aware;
* `0b18f15eb` publish the young relocation to external-root providers before a
  same-cycle major GC;
* `19cb55343` rewrite the root slice through the promotion map before the
  in-place old sweep.

None of the rerun binaries (`9fcd1b63f`, `a9ead67a1`) had any of them. The
companion report's own explanation (1) — "the fix closed the reproduction the
20-run validation exercised but not every path" — is what happened; its
explanations (2) and (3) are not needed.

## The second defect — the `<local5>` arm (JIT, and the one fixed here)

This is the failure that actually reproduced on current `dev`, and it is a JIT
bug rather than a GC one. `run_jit_callee_handler` resumes a compiled callee AT
its own exception handler instead of re-running it from entry, seeding the
handler frame with the callee's INCOMING ARGUMENTS. Its doc comment stated the
premise that made that sound:

> a compiled method whose handler reads a local first assigned inside the try
> never passes the `local_handler_reads_unsafe_local` compile gate.

`precise_handler_frames_enabled()` (default on) retired that refusal in
exchange for a precise reason-9 exceptional frame at every throwing site inside
a protected range. Only `route_jit_signal_exception` was taught to consume
those frames; the JIT-to-JIT dispatch sink was not, and kept seeding
params-only frames for exactly the population the gate no longer refuses.
Every other local was silently zeroed on resumption.

Spring Boot's `BindConverter.convert(Object, TypeDescriptor, TypeDescriptor)`
is the shape that made it visible:

```java
for (ConversionService delegate : this.delegates) {   // iterator = local 5,
    try { ... delegate.convert(...) ... }             // stored at pc 12,
    catch (ConversionException ex) { ... }            // BEFORE the range [36,58)
}
```

Its handler reads `failure` (local 4), which is what admits it through the
relaxed gate. The first delegate to throw unwound into `run_jit_callee_handler`,
which entered the handler at bci 62 with locals 4..7 zeroed; the handler falls
through to the loop back-edge, which reloads local 5:

```
NullPointerException: Cannot invoke "java.util.Iterator.hasNext()" because "<local5>" is null
BindException: Failed to bind properties under 'spring.main.allow-bean-definition-overriding' to boolean
```

That is every Spring Boot context that binds a property while a conversion
delegate throws — which is why it presented as three different-looking
failures across four classes.

**Fix** (`843b780baa`): `run_jit_callee_handler` prefers the callee's own
exceptional frame, using its locals AND its bci (a real improvement on its own
— `athrow_bci` carries no method identity, so this sink routinely received the
`usize::MAX` "unknown" sentinel and matched a handler by exception class with
no range test at all). A frame naming this method but carrying an unmappable
value fails closed. A frame naming a different method is re-stashed rather than
dropped: unlike in `route_jit_signal_exception`, its owner has not provably
left the stack. Both sinks additionally refuse the params-only fallback
whenever `cratonvm_jit::handler_reads_unsafe_local` says a handler of that
method may read past the incoming arguments, which closes the same hole for a
throw that escapes with no precise frame at all.

### How it was localised

`BasicErrorControllerDirectMockMvcTests` failed 6 of 6 JIT runs and passed 2 of
2 with `--nojit`, in about 30 seconds a run. Three levers, one run each,
named the culprit without reading any disassembly:

| lever | result |
|---|---|
| `CRATONVM_JIT_BISECT_SKIP=org/springframework/boot/context/properties/bind/BindConverter.convert` | PASS 2/2 |
| `CRATONVM_NO_JIT_PRECISE_HANDLER_FRAMES=1` | PASS 2/2 |
| `CRATONVM_DBG_RBC6=1` | named the sink: `run_jit_callee_handler ... throw_pc=18446744073709551615 handler_pc=62` |

`CRATONVM_DBG_RBC6` is the one to reach for on this family: it prints both the
compile-time gate answer (`local_handler_reads_unsafe_local=true for ...`) and
which runtime sink routed the exception. `CRATONVM_DBG_EXCFRAME=1` is its
companion, reporting every local DROPPED from a snapshot.

## Regression witness

`probes/JitHandlerResumeLocalProbe.java` — the same for-each-over-a-field with
a `try`/`catch` inside, driven through a compiled caller so the escape takes the
JIT-to-JIT sink. HotSpot `mismatches=0`; CratonVM before the fix died with
`<local3> is null` on 3 of 3 runs, after it reports `mismatches=0` on 5 of 5.

**The probe's protected range must contain nothing but invokes.** An `ldc` in
there (`Integer.MIN_VALUE` in the first draft) makes
`precise_exception_frame_sites_supported` refuse the method, it stays
interpreted, and the probe becomes a confident false null — which is exactly
what the first two drafts did.

## Validation

Executables built from `fix/springboot-conditionreport-cce-20260801`, real JDK
25 (`/data/jdk25-real-20260717/jdk-25.0.3+9`), the complete Spring Boot
4.1.0-SNAPSHOT fixture, one process per class, JIT on and default flags.

**Read the run counts, not any single run.** The build host is shared, and it
spent part of this session at load 80–145 on 16 cores with memory exhausted.
Under that, `BasicErrorControllerIntegrationTests` goes from ~290 s to ~3000 s
and starts failing on `HttpClient request timed out` — a client-side deadline,
not a VM defect. Two such runs are excluded from the table below and are named
here so nobody mistakes them for evidence either way. Check `uptime` before
trusting a red result on this fixture.

| module / class | before | after |
| --- | --- | --- |
| `spring-boot-webmvc` · `BasicErrorControllerIntegrationTests` | 5 aborts + 3 partial failures in 12 (07-31 GC report); then 23 of 26 tests failing on every JIT run | **PASS 26/26, 12 of 12 runs** |
| `spring-boot-webmvc` · `BasicErrorControllerDirectMockMvcTests` | FAIL 1/4, 6 of 6 JIT runs; PASS 2/2 `--nojit` | **PASS 4/4** |
| `spring-boot-security-oauth2-resource-server` · `OAuth2ResourceServerAutoConfigurationTests` | FAIL 1 of 52 | **PASS 52/52, 3 of 3 runs** |
| `spring-boot-cloudfoundry` · `CloudFoundryActuatorAutoConfigurationTests` | FAIL 1 of 14 | **PASS 14/14, 3 of 3 runs** |
| `spring-boot-jetty` · `JettyServletWebServerFactoryTests` | `NoSuchMethodError`+fatal `checkcast` on a worker thread | no comparator or `checkcast` event in any run; see the residual note below |

Probes:

| probe | before | after |
| --- | --- | --- |
| `probes/JitHandlerResumeLocalProbe.java` | `<local3> is null`, 3 of 3 | `mismatches=0`, 5 of 5 (HotSpot 0) |
| `probes/LocaleToStringProbe.java` | `bad=11` | `bad=0` (HotSpot 0) |

Everything above was then re-run on the branch MERGED with `origin/dev`
(`0cd98d110c`, 24 commits ahead of the branch point) — the binary that actually
lands. All green on a quiet box: both probes, `BasicErrorControllerDirectMockMvcTests`
3/3, `OAuth2ResourceServerAutoConfigurationTests` 52/52,
`CloudFoundryActuatorAutoConfigurationTests` 14/14,
`JettyServletWebServerFactoryTests` **113/113 twice**, and
`BasicErrorControllerIntegrationTests` 26/26 four times. This re-run is the one
to trust: a merge is exactly where a fix like this gets silently relocated or
dropped.

`dev` then landed its own fix for the same JIT defect and its own retirement of
the GC report, so the whole set was run a THIRD time, on the resolved merge of
both — the tree that is actually pushed — on a quiet box (load ~10, against the
80–145 the middle rounds ran under):

| class | result |
| --- | --- |
| `BasicErrorControllerDirectMockMvcTests` | PASS 4/4, 3 runs |
| `BasicErrorControllerIntegrationTests` | PASS 26/26, 4 runs |
| `JettyServletWebServerFactoryTests` | PASS 113/113, 2 runs — including `whenServerIsShuttingDownGracefullyThenNewConnectionsCannotBeMade` |
| `OAuth2ResourceServerAutoConfigurationTests` | PASS 52/52 |
| `CloudFoundryActuatorAutoConfigurationTests` | PASS 14/14 |
| both probes | clean |

One unit test is red on that tree and is NOT from this work:
`x64::tests::push_stack_refuses_to_cross_spill_limit`. `jit/src/x64.rs` is
byte-identical to `origin/dev` here — this branch never touches it.

Unit tests on the fix branch: `cratonvm-jit --lib` 1228/0, `cratonvm-vm --lib`
2330/0. `cratonvm-native-builtins --lib` is 3202 passed / 2 failed, and **both
failures pre-date this work** — `panama::tests::test_85_4_upcall_handle_and_invoke`
and `tls_deny::tests::every_plaintext_base_overload_is_accounted_for` fail
identically with these changes stashed.

`JettyServletWebServerFactoryTests` had two failures left, and both are now
closed — one here and one on `dev`, in parallel:

* `localeCharsetMappingsAreConfigured` is fixed here (`6c2a8a677d`).
  `Locale.toString()` returned `""` for every real-JDK `Locale`, and Jetty keys
  its locale→encoding map on exactly that string, so a mapping registered for
  GERMAN answered a lookup for ITALIAN. The GC report's own retirement flagged
  this as "a separate regression that landed on `dev` in the same window" and
  left it open; this is its root cause.
* `whenServerIsShuttingDownGracefullyThenNewConnectionsCannotBeMade` was
  root-caused and fixed on `dev` while this branch was in flight, and it is not
  a Jetty problem at all: `ServerSocketChannel.close()` left a duplicate OS
  handle open, because `ssc_accept` had `try_clone()`d the listener before its
  poll loop. A connection arriving inside the 10 ms poll window was accepted
  and served. See
  [`springboot-basicerrorcontroller-checkcast-abort-20260731-FIXED.md`](springboot-basicerrorcontroller-checkcast-abort-20260731-FIXED.md).
  An earlier draft of this document filed it as a new OPEN report; that was
  written before the fix landed and has been withdrawn rather than published
  stale.

## Affected classes

- `module/spring-boot-webmvc` — `org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerIntegrationTests` (the original 2026-07-28 report; the GC abort, and from 2026-07-31 the `<local5>` arm)
- `module/spring-boot-webmvc` — `org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerDirectMockMvcTests` (the `ConditionAndOutcomes` cast on the report read-back path, and the `<local5>` arm — the fastest repro of the two, ~30 s a run)
- `module/spring-boot-security-oauth2-resource-server` — `org.springframework.boot.security.oauth2.server.resource.autoconfigure.OAuth2ResourceServerAutoConfigurationTests` (the `ConditionAndOutcomes` cast on the `recordConditionEvaluation` WRITE path, failing context startup outright)
- `module/spring-boot-cloudfoundry` — `org.springframework.boot.cloudfoundry.autoconfigure.actuate.endpoint.servlet.CloudFoundryActuatorAutoConfigurationTests` (same write-path manifestation)
- `module/spring-boot-jetty` — `org.springframework.boot.jetty.servlet.JettyServletWebServerFactoryTests` (the GC abort's second call site, `HttpCookie.from`; plus two residuals of its own, one fixed here and one re-filed)

## What this does NOT close

One item travelled with these reports and is neither fixed nor invalidated
here. It was re-filed rather than retired with this document:

* The JIT code-buffer overflow flood
  (`JIT try_patch_i32: offset out of bounds; marking buffer overflowed`, 3552
  in one 4-test class) →
  `docs/internal/fixed-suite-bugs/jit-ir-tier-code-buffer-estimate-20260801-FIXED.md` (FIXED 2026-08-01: the estimate was the IR tier's and is now fitted to a 1664-compile census).
  It is a silent de-optimization, not corruption. The re-filed version corrects
  the attribution: `CRATONVM_DBG_IR_BAILOUT=1` shows 54 optimizing-tier
  `code_buffer_exhausted` bailouts against 5 from `x64::compile`, so it is the
  IR tier's `nodes*32 + calls*448 + 1024` estimate, not `x64.rs`'s.
(The Jetty graceful-shutdown residual was going to be the second entry here.
It was fixed on `dev` first — see the Jetty paragraph above — so there is
nothing left to file.)

One further loose end from the retired
`basicerrorcontroller-jit-only-failure-20260731.md` is recorded but NOT
re-filed: the `DeferredLogFactory.getLog(Class)` receiver mix-up
(`NoSuchMethodError: java/lang/Class.getLog(...)`, the argument taken as the
receiver) was last seen on a binary at `351218f44`, has not been observed since
`7f1b1f263`, and was never root-caused. If it reappears, it is a separate
defect — do not assume the handler-frame fix here covers it.
