# CRATONVM_JIT_OSR regression cluster — value corruption across OSR entry-state reconstruction

**Status:** ARCHIVED / RESOLVED on current `dev` (2026-07-04). The issue was
retired as a default-on blocker by three later fixes plus one reclassification:
OSR compiles now seed reference parameters into the oop mask, collision-shaped
primitive locals round-trip bit-exact through OSR snapshots, unsafe dead-local
entries are rejected before entering the trampoline, and the Tomcat
DirResourceSet case was refuted as a non-OSR stale-baseline regression. The
historical Tomcat sweep remains below for context; its original default-off
recommendation is superseded by `vm/src/runtime/env_cache.rs`, where
`CRATONVM_JIT_OSR` now defaults on and `CRATONVM_JIT_OSR=0` is the opt-out.

**Historical severity:** high (silent value corruption, one case a hang).
**Flag under test:** `CRATONVM_JIT_OSR` (was default **off** at the time of this
investigation; now default **on**).
**Tasks:** [task_4d21c2f7](#3-testdirresourceset-family-6-classes--case-sensitivity-check-bypassed) (DirResourceSet case-sensitivity bypass), [task_24621ee7](#4-testwebxmlordering--indefinite-hang-compactvalue-nan-box-collision) (TestWebXmlOrdering hang / CompactValue collision).

## Summary

`CRATONVM_JIT_OSR` is a deliberate default-off gate: its doc comment already
warns that on-stack-replacement (OSR) entry-state reconstruction is "not yet
fully hardened" and that Xerces XSD DFA construction is known to read a
corrupted value and loop forever under it. To find out how broad the risk
actually is, we ran the complete Apache Tomcat 12.0 JUnit suite (651 classes,
`apps/tomcat` in this repo) with `CRATONVM_JIT_OSR=1` set and compared every
result against a same-commit non-OSR baseline.

**Method:**
- Built `dev@46e56861` in two independent worktrees (Windows: local box;
  Linux: Azure host `20.84.156.31`, `/home/victor/wt-osr120full`) with
  `cargo build --release -p cratonvm-cli --bin cratonvm`.
- Sanity probe (Linux, bt18 bintrees benchmark): checksum `68332206` identical
  with OSR on and off, no GC-corruption warnings — confirms the binary itself
  is sound outside the Tomcat-specific findings below.
- Full 651-class Tomcat suite on Windows, real JDK, JIT on, `CRATONVM_JIT_OSR=1`,
  120s per-class timeout, `-Parallel 2`. Run name `osr120full`.
- Every non-PASS class was cross-referenced against the same-commit non-OSR
  baseline run (`overnight0629c`, all 651 classes, JIT on, OSR unset). Only
  classes that **PASS without OSR** and **fail/hang/crash with OSR** count as
  candidate regressions.
- Every candidate was then re-run **serially in isolation** (`-Parallel 1`,
  single class) to rule out parallel-load contention/timing noise before
  being treated as a genuine finding.

## Results

| status | count (OSR on, 651/651) |
|---|---:|
| PASS | 407 |
| FAIL | 82 |
| HANG | 144 |
| CRASH | 8 |
| NOSUMMARY | 10 |

**All 8 CRASH and all 10 NOSUMMARY classes are pre-existing** (same status
without OSR) — the known `TestHttpServletDoHead*` GC non-moving-sweep
corruption family, `TestDefaultServletRfc9110Section13`, and
`TestMulticastPackages` (tracked separately). **Zero new crashes caused by
OSR.**

Of the FAIL/HANG set, 17 classes were baseline-PASS-now-broken candidates.
**13 were ruled out** as timing/contention noise after isolation (listed
below). **4 are confirmed, reproducible, genuine OSR regressions** — all four
share a family resemblance: a value silently goes wrong (null, mis-typed, or
never-resolving) immediately after what looks like an OSR transition, rather
than a clean crash.

## Confirmed regressions

### 1. `jakarta.el.TestBeanSupport` — NPE on reflection metadata
```
java.lang.NullPointerException: Cannot invoke "java.lang.reflect.Method.getReturnType()"
because the return value of "jakarta.el.BeanELResolver$BeanProperty.getWriteMethod()" is null
```
Methods `testOverLoadedWithGetABean`, `testOverLoadedWithGetAABean`. PASSes
without OSR (`overnight0629c`: PASS). `BeanProperty.getWriteMethod()` — a
cached `Method` reference populated during bean-property introspection —
comes back null under OSR.

### 2. `jakarta.el.TestImportHandlerStandardPackages` — spurious file-not-found
```
java.io.UncheckedIOException: java.nio.file.NoSuchFileException
  at jakarta.el.TestImportHandlerStandardPackages.checkPackageClassList(TestImportHandlerStandardPackages.java:58)
```
Method `testClassListsAreComplete`. PASSes without OSR.

### 3. `TestDirResourceSet` family (6 classes) — case-sensitivity check bypassed
Affects `TestDirResourceSet`, `TestDirResourceSetInternal`,
`TestDirResourceSetMount`, `TestDirResourceSetMountTrailing`,
`TestDirResourceSetReadOnly`, `TestDirResourceSetVirtual` — all inherit the
same shared test from `AbstractTestResourceSet`:
```java
// AbstractTestResourceSet.java:164-167
public final void testGetResourceCaseSensitive() {
    WebResource webResource = resourceRoot.getResource(getMount() + "/d1/d1-F1.txt"); // wrong case
    Assert.assertFalse(webResource.exists());
}
```
Only `d1-f1.txt` (lowercase) exists on disk; the assertion expects a
case-sensitive miss. Under OSR, `webResource.exists()` incorrectly returns
`true` — the case check is silently bypassed. **Verified non-flaky**: reproduced
serially in isolation (`-Parallel 1`, single class, ~6s, no contention
possible). PASSes without OSR. Tracked as `task_4d21c2f7`.

### 4. `TestWebXmlOrdering` — indefinite hang, CompactValue NaN-box collision
`org.apache.tomcat.util.descriptor.web.TestWebXmlOrdering` (web.xml
absolute-ordering parsing). PASSes without OSR in 66.4s. With OSR on it never
completes — reproduced at 120s under load and again at **150s in full
isolation** (`-Parallel 1`), ruling out contention.

The isolated run's log is the most informative evidence in this cluster —
it shows the actual failure mechanism, not just a symptom:
```
WARN cratonvm_gc::gen_heap: non-moving sweep: forwarded young object at offset 56492024
  has non-old-gen target 0x20000000000 — retaining span, not freeing
  (repeats identically every ~55-60s — i.e. once per GC cycle, same offset, never resolving)
WARN [org.apache.tomcat.util.descriptor.web.WebXml] Used a wrong fragment name [z] at web.xml absolute-ordering tag!
CompactValue: first long↔object NaN-box collision degraded to Value::Long
  (SUB_OBJECT-patterned primitive long reached a context-free decoder).
  Subsequent collisions are counted by object_degradation_count() but not logged.
```
CratonVM's `CompactValue` uses NaN-boxing to pack primitives and object
references into one word. The collision line means a bit pattern that should
decode as an object reference is ambiguous with a primitive `long`, and the
decoder had to guess wrong — producing a corrupted reference that the
non-moving GC sweep then can never resolve, stalling forever at the same
heap offset. This is exactly the "entry-state reconstruction not fully
hardened" risk documented in `osr_backedge_enabled()`'s doc comment: an OSR
transition is very likely rehydrating a live stack-slot/local-variable value
with the wrong type tag. Tracked as `task_24621ee7`, which also asks the
assignee to check whether this is the **shared root cause** behind
findings #1–#3 above (all are "a value comes back wrong right after an OSR
jump" symptoms).

## Ruled out — timing/contention noise, not OSR bugs

These 13 classes went FAIL/HANG with OSR on and PASS in the baseline, but did
**not** reproduce cleanly under isolation, or showed the established
network/timing-flake signature (`expected:<200> but was:<-1>`, connection
reset, or inconsistent symptom between parallel and serial runs) already
characterized elsewhere in this suite's investigation:

- `org.apache.catalina.core.TestApplicationContextGetRequestDispatcher` —
  FAILs under `-Parallel 2` (`IllegalThreadStateException` on engine
  startup, plus a stray double-`stop()` log line), but **HANGs** instead
  under serial isolation — inconsistent symptom, indicates a genuine race
  rather than a clean OSR defect.
- `org.apache.catalina.startup.TestStartupIPv6Connectors` (`expected:<200>
  but was:<-1>`)
- `org.apache.catalina.startup.TestTomcatNoServer`
- `org.apache.coyote.http2.TestHttp2Section_4_2` (`SocketException:
  Connection reset`)
- `org.apache.coyote.http2.TestHttp2Section_6_7`
- `org.apache.coyote.http2.TestHttp2Timeouts`
- `org.apache.coyote.http2.TestHttpServlet`
- `org.apache.coyote.http2.TestRfc9218`

## Historical recommendation (superseded)

**Keep `CRATONVM_JIT_OSR` default-off.** This sweep confirms the gate's own
documented risk is real and not merely theoretical: 4 distinct, reproducible
regressions surfaced from one 651-class suite, all in the same failure
family (silent value corruption across an OSR transition), with one
(`TestWebXmlOrdering`) pointing at a concrete mechanism (`CompactValue`
NaN-box type-tag collision) rather than a vague "sometimes it's wrong."
Revisit the default once `task_4d21c2f7` and `task_24621ee7` land — and
re-run this same full-suite sweep as the acceptance check before flipping
the gate on by default.

Superseded 2026-07-04: the later fixes and reclassification listed at the top
retire this note as a known issue, and OSR is now the default with an explicit
`CRATONVM_JIT_OSR=0` opt-out.
