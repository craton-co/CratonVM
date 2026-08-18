---
name: tomcat-jetty-servletwebserverfactorytests-300s-budget-overrun-FIXED-20260811
description: CLOSED 2026-08-11. Both classes now run to completion with ZERO failures - Tomcat 132/132 in 420s, Jetty 113/113 in 407s - where this page last recorded 90 failures on one and "never reported a single test" on the other. The throughput residual it was reduced to had a real, findable defect underneath it: every java.util.jar.JarFile accessor stat'ed the file to rebuild its cache key, which Jasper's TLD scan pays once per jar entry per container start. Fixing that took Tomcat 574s to 420s and Jetty 533s to 407s. What is left is an ordinary 10-14x interpreter-throughput ratio, and both classes now carry a measured per-class budget instead of being reported as HANGs.
metadata:
  type: fixed-suite-bug
  area: springboot, tomcat, jetty, throughput, jar, suite-harness
---

# `TomcatServletWebServerFactoryTests` / `JettyServletWebServerFactoryTests` — closed

**CLOSED 2026-08-11.** Filed 2026-08-07 as
`known-issues/springboot/tomcat-jetty-servletwebserverfactorytests-300s-budget-overrun-20260807.md`.

The page passed through three states. The `port 8080` failures were fixed
2026-08-10 (`hashmap-get-misses-a-key-its-own-entryset-yields-FIXED-20260810.md`).
What it was reduced to — "steady forward progress, not a stall; the class
needs ~2x the per-class budget" — was correct as a description and incomplete
as a diagnosis: a single defect was carrying about a quarter of the wall
clock, and it was reachable.

## Where both classes are now

Single class, alone on an otherwise-quiet Windows host, `-Xmx 2g`, default
collector (ZGC), 2026-08-11. `A` is `dev@9f9c03f20`; `B` is the same tree plus
the `JarFile`-accessor fix below.

| class | A | B | HotSpot 25 | B vs HotSpot |
|---|---:|---:|---:|---:|
| `TomcatServletWebServerFactoryTests` | 574.3 s, 132 tests, **0 failed** | **420.2 s**, 132 tests, **0 failed** | 41.1 s, 129 tests, 0 failed (3 skipped) | 10.2x |
| `JettyServletWebServerFactoryTests` | 533.5 s, 113 tests, **0 failed** | **407.0 s**, 113 tests, **0 failed** (2 skipped) | 28.6 s, 111 tests, 0 failed (4 skipped) | 14.2x |

Against what this page last recorded, that is:

* **Tomcat: 90 failures → 0.** The 2026-08-10 update measured "813.7 s with
  132 tests — and 90 of them fail", 82 of those being
  `ConnectorStartFailedException … port 8080`. Zero failures now, on either
  binary, confirming the `HashMap` identity-hash fix end-to-end rather than by
  the absence of a log line.
* **Jetty: settled.** The page left it explicitly unsettled — a 2400 s
  `--stack-sample-ms=200` run "produced `tests=0` … never reported a single
  test, having spent ~20 minutes inside one `ContextHandler`/TLD-scan start".
  On an unsampled idle host it reports all 113 in 407 s. That ~20-minute
  single-context start was the TLD scan, which is exactly what the fix below
  addresses, amplified by the 14 concurrent CratonVM processes that run was
  carrying.

## The defect underneath the throughput gap

Full write-up:
`fixed-bugs/jarfile-accessors-stat-the-file-on-every-call-FIXED-20260811.md`.
In short: `java.util.jar.JarFile`'s natives resolve everything through a
`(path, mtime)`-keyed cache, and the **mtime was re-derived with a live
`std::fs::metadata` on every accessor call**. On Windows that opens a file
handle — 20-54 us measured, against ~0.3 us for the cache lookup it guarded.

That lands on these two classes specifically because Jasper's TLD scan drives
`org.apache.tomcat.util.scan.JarFileUrlJar.nextEntry()`, which on a
multi-release jar calls `JarFile.getJarEntry(name)` **once per entry**, and
each of those reaches the cache up to three times. Both classes start a fresh
embedded container per test — 121 of them for Tomcat over a 130-jar test
classpath, 38 of which are genuinely multi-release.

`probes/TldJarScanProbe.java` reproduces that walk standalone: **789-2807 ms
before, 140-155 ms after, HotSpot 40-58 ms** — and the before column grew
every round while the after column is flat.

This is also what the page's own §"Root cause — not identified in this pass"
was pointing at with its candidate 1 (reflection-heavy per-cycle bean
introspection) and candidate 2 (per-cycle keystore/TLS setup). Neither was
right; the third suggestion — "a direct `--stack-sample-ms 200` capture of
either class in isolation would settle this in one run" — was, and is how this
was found. A 3-second-interval sample of the whole Tomcat class put 47 of 236
main-thread leaf frames in `JarFileUrlJar.nextEntry` and 35 more in
`TldScanner$TldScannerCallback.scan`, with the `nextEntry` samples landing
36/47 at the bytecode offset immediately after `JarFile.getJarEntry`.

## The two things this page said that were wrong, and why

**"Neither log shows a single outlier cycle … the overrun here is uniform
per-cycle cost accumulated over many cycles, not one stuck operation."** Half
right. There is no single stuck operation, but the per-cycle cost was not
uniform either — it *grew*, because the stat cost grows as the process ages
(see the probe's round-over-round column). A run whose cycles get slower looks
like a hang from the outside long before it is one.

**The 2026-08-06/08-10 runs were on the Generational collector, and one of
this investigation's own runs shows why that matters.** A Generational run of
the Tomcat class on 2026-08-11 took 1669.8 s and reported 1 failure — but
1163.2 s of that was a *single* gap between two log lines, and the `.err.log`
across that gap is `[moving-young] fallback #512 … #1024 … #2048` followed by
a flood of `young non-moving sweep: the header at this offset claims an extent
that SUBSUMES a live (marked) object`. That is the separately-tracked
non-moving-sweep fallback spiral
(`known-issues/retired/moving-young-fallback-four-springboot-classes-RETIRED-20260818.md`),
not anything about these classes, and it does not occur under the current
default collector. The one failure it produced —
`shouldUpdateSslWhenReloadingSslBundles`, `SSLHandshakeException: handshake
read: connection aborted` — did not reproduce on any ZGC run, and the test
passes standalone in 12.2 s. **Pin the collector to the current default when
re-measuring these classes**; a Generational arm measures a different open
issue.

## What is left, and where it lives

10.2x and 14.2x HotSpot, with zero failures.

The largest named item left in the pre-fix profile, after the jar walk, is
Xerces/Digester parsing the `.tld` files themselves — 21 samples in
`XMLDocumentFragmentScannerImpl$FragmentContentDriver.next`, 17 in
`Digester.startElement`, 9 in `Digester.endElement`, and 76 of the
`TldScannerCallback.scan` samples sitting at the offset right after
`parseTld`. That is ordinary interpreter throughput on an XML parser, owned by
`known-issues/tomcat/!webapp-deploy-annotation-scan-interpreted-226x.md`.
Stated as an expectation, not a measurement: no post-fix profile was taken
(the host had picked up another session's full suite by then), so the ranking
after the fix is inferred from removing the jar-walk term, not observed.

Nothing about that is specific to these two classes, and nothing left on this
page is.

Both classes now carry a measured budget in
`apps/spring-boot-suite-runner/run-spring-boot-suite.ps1`'s
`Get-EffectiveClassTimeoutSec`, the same treatment the ~20 entries already
there get: they are CPU-bound, they finish, and reporting a false HANG at
300 s hides their real result. The ratio is named in the entry's comment and
is not papered over by it.

## Affected classes

- `module/spring-boot-tomcat` —
  `org.springframework.boot.tomcat.servlet.TomcatServletWebServerFactoryTests`:
  **PASS**, 132/132, 420.2 s.
- `module/spring-boot-jetty` —
  `org.springframework.boot.jetty.servlet.JettyServletWebServerFactoryTests`:
  **PASS**, 113/113 (2 skipped), 407.0 s.
