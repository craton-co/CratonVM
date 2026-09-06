# G1 only: a forwarding-assert abort (evac worker panic) plus three SIGSEGV clusters in unrelated-looking functions, 15 crashes total

| | |
|---|---|
| **Status** | OPEN. The evac-worker panic cluster is confirmed **not** a straightforward regression of the exact line fixed in `g1-parallel-evacuation-cas-loser-tagged-mark-word-FIXED-20260807` (that fix is present and correct in the code that built this binary) — but it trips the same assert, in the same two-file panic-propagation shape, and the `make_forwarded` call-site census that doc closed is now stale. The three SIGSEGV clusters are a real, reproducible, class-list-stable phenomenon; whether they share a root cause with the evac panics is an open, honestly-unresolved question — evidence for and against is below. |
| **Scope** | 15 CRASH-classified classes, G1 arm only, 0 in the same run's Generational or ZGC arms. |
| **Measured** | 3-GC-shard (640-class) Tomcat run on Azure, binary built from dev tip `7acc0b27c` (2026-09-05 ~12:07 UTC). G1 shard-0. |

## The 15 crash classes, clustered by fault signature

```
$ ssh azureuser@20.80.105.49 "awk -F, '\$4==\"CRASH\"{print \$1}' \
    /data/cratonvm/apps/tomcat/.suite/results/g1-3gc-20260905/shard-0/results.csv"
```
returns exactly these 15, which resolve into four clusters:

**Cluster A — SIGSEGV, static offset `0x9f46d8`, symbol `cratonvm_types::flags::runtime_var_os::<&str>`** (5 classes):
`TestHttpServletDoHeadInvalidWrite1024ValidWrite1024`, `TestAsyncContextStateChanges`,
`TestGenerator`, `TestJspDocumentParser`, `TestELInterpreterTagSetters`.

**Cluster B — SIGSEGV, static offset `0x9f4a08`, symbol `<cratonvm_types::field_layout::VersionCache>::find`** (4 classes):
`TestMapperWebapps`, `TestCompiler`, `TestEncodingDetector`, `TestJspConfig`.

**Cluster C — SIGSEGV, static offset `0xa74dc3`, symbol `cratonvm_gc::gen_heap::plan_object_alloc`** (1 class):
`TestDefaultInstanceManager`.

**Cluster D — panic/abort, `types/src/heap_types.rs:1529` then `gc/src/evac_pool.rs:218`** (5 classes):
`TestFormAuthenticatorB`, `TestHostConfigAutomaticDeploymentUpdateWarOffline`,
`TestHostConfigAutomaticDeploymentXmlExternalDirXml`,
`TestHostConfigAutomaticDeploymentXmlExternalWarXml`,
`TestHostConfigAutomaticDeploymentDeleteB`.

All four static offsets/symbols were re-verified directly against the actual
`.log` files on Azure (fault pc, ELF text mapping, `addr2line -f -C -e
target/release/cratonvm`), not taken on faith from the earlier pass.

## Cluster D: the evac-worker panic

Every one of the 5 classes shows the identical assert text:

```
thread 'g1-evac-N' panicked at types/src/heap_types.rs:1529:9:
forwarding target must have its low 2 bits clear (>= 4-byte aligned)
thread 'main-vm' panicked at gc/src/evac_pool.rs:218:13:
fatal runtime error: failed to initiate panic, error 5, aborting
```

(In 2 of the 5, only the `main-vm` re-panic line appears because the worker's
own panic scrolled out of the head of what was grepped; the assert text is
present in all 5 when grepped directly.)

`evac_pool.rs:218` is `panic!("g1 parallel-evac worker panicked")`, which fires
when `RetireOnExit`'s guard observes `st.panicked` set — this is exactly the
"second defect" (panic-safe worker retirement) that
`g1-parallel-evacuation-cas-loser-tagged-mark-word-FIXED-20260807`
describes and fixed: a worker panic is converted into a loud, main-thread abort
instead of a silent hang. **That half of the Aug-7 fix is doing exactly its
documented job here** — it is not the bug, it is the mechanism correctly
surfacing whatever the real bug below is.

### Is this the exact Aug-7 defect regressing? No — verified against the current code.

The Aug-7 fix's root cause was: `SharedEvac::evacuate`'s CAS-loser arm cast the
raw (still-tagged) mark word straight to a pointer instead of decoding it via
`ObjectHeader::forwarding_target`, producing an address exactly `+3`
(`MARK_FORWARDED`) past the real one.

Reading the current `gc/src/g1.rs` (the exact code the crashing binary was
built from — `git diff 7acc0b27c HEAD -- gc/src/g1.rs` is a 47-line diff
confined entirely to a `#[cfg(test)]` module added *after* the build, so the
evacuation code itself is byte-identical to what ran on Azure):

- **Fast-path re-forward** (`g1.rs:1097-1099`): decodes via
  `ObjectHeader::forwarding_target(observed)`. Correct.
- **Evacuation-failure self-forward, CAS-loser arm** (`g1.rs:1168-1169`):
  `Err(actual) if ObjectHeader::is_forwarded_mark(actual) => Some((ObjectHeader::forwarding_target(actual), false))`.
  Correctly decoded.
- **Normal-copy CAS-loser arm** (`g1.rs:1243-1262`) — the exact arm the Aug-7
  doc names: `Err(winner) if ObjectHeader::is_forwarded_mark(winner) => { let
  target = ObjectHeader::forwarding_target(winner); ... }`. Correctly decoded,
  and the comment block above it (lines 1221-1242) explicitly narrates the
  Aug-7 defect and states the fix is in place. There is also a second,
  Aug-26-dated fix note here (`forwards.push` now runs on this arm too, a
  follow-on to Aug-7 for a `pointer_map` omission) — i.e. this arm has been
  touched and re-verified *after* Aug-7, not just left alone.

**No raw `as *mut u8` cast on any CAS-loser arm exists in the current
`SharedEvac::evacuate`.** This is not a regression of that exact line.

### But the failing assert is on the *value being installed*, not a decode

Both asserts inside `ObjectHeader::make_forwarded` (`types/src/heap_types.rs:1529-1536`)
check the `target` argument itself — "low 2 bits clear" and
`plausible_heap_pointer` — and the message captured from every one of the 5
logs is specifically the **first** one (low-2-bits), which is exactly the
shape a not-yet-decoded `target | MARK_FORWARDED` word would trip if it were
ever fed back into `make_forwarded` as if it were clean. Since the two
CAS-loser arms that read a raw CAS-failure word are confirmed correctly
decoded, the value reaching `make_forwarded` at the point of failure is either:

- the **self-forward candidate** `old` (`old_ptr as usize`, `g1.rs:1154`) —
  which would mean `old_ptr` itself was not 8-aligned, i.e. `evacuate()` was
  called on a bad/corrupt candidate address; or
- the **fresh destination** `new_addr` (`g1.rs:1211`), returned by
  `self.tlab_alloc(dest_tlab, obj_size)` — which would mean the TLAB allocator
  handed back a misaligned or implausible address.

Neither is the Aug-7 defect. Both are plausible **new** defects, and this
investigation did not have the means (no rebuild, no repro harness, read-only
Azure access) to determine which. **This is filed as open, not resolved.**

### The `make_forwarded` audit the Aug-7 doc closed is now stale regardless

That doc's closing claim: *"`make_forwarded` has exactly two call sites in the
workspace, both in `g1.rs`... the audit is closed."* As of this checkout,
`make_forwarded` has production call sites in:

- `gc/src/g1.rs`: lines 1154, 1211, 8231, 8340 (four, not two — the extra two
  are the *serial* evacuator `evacuate_object`, which stores directly with no
  CAS and so isn't subject to the loser-decode bug, but wasn't part of the
  two-site count either).
- `gc/src/gen_evac.rs`: lines 1111 (production CAS-loser arm, `:1125` —
  correctly decoded via `is_forwarded_mark`/`forwarding_target`, confirmed by
  reading it) and 1664 (a test fixture simulating a racing winner, not
  production code).
- `gc/src/zgc.rs:8615`: a ZGC forwarding call site, outside G1 entirely.

None of the sites read show the raw-cast defect. But the census itself having
grown from 2 to 8+ call sites since the audit that declared it "closed" means
that closure claim no longer describes the codebase, independent of whatever
turns out to be causing today's crashes. Worth a fresh audit pass the next
time anyone touches evacuation code.

## Clusters A/B/C: three SIGSEGVs in functions that aren't obviously GC-unsafe

All three share `slot[r10]: UNREADABLE (r10 is not a readable pointer)` in
their register dump, confirmed by reading the actual crash reports (not just
the earlier pass's summary):

```
# Cluster A (TestAsyncContextStateChanges): addr=0x7f1671233000  r10=0x308d0
# Cluster B (TestMapperWebapps):            addr=0x70f6fd362000  r10=0x7ff77a0ff8000000
# Cluster C (TestDefaultInstanceManager):    addr=0x636a61636a34  r10=0x0
```

Two of the three fault *addresses* (A and B) are themselves page-aligned
(end in `000`) — consistent with stepping just past the end of a mapped
region/table rather than dereferencing a purely-random garbage word. Cluster
C's fault address is not page-aligned and looks like ordinary heap garbage.
The `r10` values themselves are not a shared constant across the three (they
differ completely), so "UNREADABLE" here is the crash reporter's routine
per-crash diagnostic, not by itself proof of one shared corrupting write —
it's suggestive, not conclusive.

**On `plan_object_alloc` "running under G1"**: the earlier pass read this as
possible evidence of a GC-selection/dispatch bug (Generational code invoked
while G1 was configured). Reading `gc/src/gen_heap.rs:18228-18248`, that's not
supported: `plan_object_alloc` is a GC-agnostic **object-layout** helper
(decides body size and `GC_FLAG_COMPACT` vs. legacy shape from class
metadata) called from every allocation path — JIT helpers, the interpreter,
lambda/proxy capture — regardless of which collector is active. It happens to
live in a file named `gen_heap.rs` for organizational/historical reasons, not
because it's gated to the Generational arm. A crash inside it under G1 is
therefore not itself evidence of cross-collector dispatch confusion; it reads
metadata (class id, layout table) that could be corrupted by something else.
**This narrows, rather than supports, the "wrong GC path" framing** the
original note raised — withdraw that specific interpretation.

### Process isolation rules out cross-*class* corruption; workload correlation is the real signal

Each class is a separate OS process (confirmed: every crash log has a
distinct `pid=`), so a corrupting write from one class's process cannot
literally propagate into a different class's process — the "downstream
corruption from cluster D" framing, if true at all, can only mean **within one
class's own run**, not across the shard.

What *is* real, from checking each crash class's row position and timestamp in
`results.csv` (sequential, alphabetically-ordered, one class after another):

- The tightest cluster is rows 451/454/455/456/457
  (`TestCompiler`→`TestEncodingDetector`→`TestGenerator`→`TestJspConfig`→`TestJspDocumentParser`),
  all crashing within a 12-minute, 7-row span (17:06-17:18), and **all five are
  `org.apache.jasper.compiler`/`org.apache.jasper.optimizations` classes**
  (clusters A and B mixed). Checking the same package's *other* 6 classes in
  that exact row range (`TestAttributeParser`, `TestELInterpreterFactory`,
  `TestELParser`, `TestJspReader`, `TestJspUtil`, `TestJspUtilMakeJavaPackage`,
  `TestValidator`) — **all 6 PASS clean**, interleaved between the crashes.
  So it isn't "any Jasper test crashes" — but 5 of 11 `jasper.compiler` classes
  crashing (45%) against an overall shard crash rate of 15/640 (2.3%) is a
  ~19x enrichment. That's a genuine, worth-investigating correlation with the
  JSP-compiler workload shape (heavy string/char-array parsing and codegen,
  plausibly heavier/differently-shaped GC pressure), not proof of a specific
  causal chain.
- `TestAsyncContextStateChanges` (cluster A, row 154, 14:27:12) and
  `TestDefaultInstanceManager` (cluster C, row 156, 14:27:41) are 2 rows and 29
  seconds apart — different processes, different PIDs, so this is scheduling
  proximity only, not shared state.
- The cluster-D (evac panic) classes are scattered across a *different* time
  window (14:09, 15:38-16:06) than the tight A/B cluster (17:06-17:18) — they
  do not immediately precede or follow the Jasper cluster in this shard's
  order. There is no row/time adjacency between cluster D and clusters A/B/C
  in this data.

**Conclusion on the cross-cutting hypothesis**: the "shared root cause"
reading, if correct, has to be a **within-process** version — some class's own
run hits a G1 evacuation race that produces a bad-but-plausible (assert-passing)
forwarding target rather than one that trips `make_forwarded`'s checks, and a
later allocation/lookup in the *same* run reads through it. That would explain
why unrelated-looking functions (an env-var reader, a field-layout version
cache, an alloc-shape planner) are the ones landing on the stack — they're
just "whichever code touches the corrupted memory next" — without requiring
any of them to be individually GC-unsafe. **This is not established here**:
none of the 10 SIGSEGV-cluster logs shows a `heap_types.rs:1529` assert or an
`evac_pool.rs` panic anywhere in the same run, so if this is what's happening,
it's a *silent* variant of the same underlying defect that doesn't happen to
trip the loud assert. The Jasper-family enrichment is real and reproducible;
whether it's explained by "more GC pressure → more chances at a rare silent
evac defect" or by something entirely unrelated to evacuation was not
determined in this pass.

## A LOCAL reproducer for the Cluster D assert — and why it stopped reproducing

**2026-09-05, Windows/RTX 2060 box.** The same assert fires outside
Tomcat, on a GPU fixture that runs in about forty seconds:

```
panic: forwarding target must have its low 2 bits clear (>= 4-byte aligned)
  types/src/heap_types.rs:1562        thread="main-vm"
[PANIC_IN] GpuResidencyGc.main pc=127

bash bench-gpu/residency-gc.sh          # or, one launch:
cratonvm --gpu --gpu-min-work 64 -Xmx64m -XX:+UseG1GC     -cp test_classes/gpu GpuResidencyGc 0 1024 60
```

Same assert text, same file, same **G1-only** scope this page reports.
One difference to keep in view: this fires on `main-vm`, while Cluster D
panics on an evac worker via `gc/src/evac_pool.rs`. Whether that is the
same defect on a different thread or a second path to the same assert is
**not established**.

It was found by accident — `bench-gpu/residency-gc.sh` routes each arm's
stderr into a temp directory it deletes, so the failure presented as an
empty arm with two mismatched checksums, not as a panic.

### It is NOT reliably reproducible, and one claim here was retracted

Observed roughly five times inside a single evening window, then **0 in
about 500 launches** afterwards — across binaries built both before and
after `587acb50e` (the stale-TLAB-skip-span fix), so that fix is not the
explanation either.

An intermediate reading that host load amplifies it does **not** hold up.
It was measured at 3/120 loaded against 0/120 quiet, which looked
conclusive (p ~ 2e-5). Re-running the *identical binary* under the
*identical* synthetic load later gave **0/150**. The difference between
those windows is what else was on the box: the first ran alongside two
other sessions' real VM workloads, the second alongside twelve CPU
spinners. So whatever forces it is not CPU occupancy — more likely
concurrent memory/GC pressure from real workloads, which a spin loop does
not reproduce. Recorded as a refuted hypothesis rather than deleted,
because the refutation is the useful part: **do not size a burn-in
against CPU load.**

That also bears on this page's own suggestion of re-running the Jasper
`compiler` package alone to test whether the ~45% crash rate was "an
artifact of this one run's host load/timing". On this evidence an
isolated re-run may well come back clean without meaning anything.

### The assert now names its provenance

`ObjectHeader::make_forwarded` was `#[track_caller]`-annotated and its
messages now carry the offending values, so the next firing — here or on
Azure — reports the **call site** rather than `heap_types.rs`, plus
`target`, its low bits, and `prev`. That directly separates the two
candidates this page names as its most useful next step: G1 passes
`old`/`old_addr` when self-forwarding a CAS loser (`g1.rs:1191`, `:8289`)
and `new_addr`/`new_ptr` for a copy destination (`g1.rs:1248`, `:8398`) —
i.e. bad `old_ptr` candidate versus bad `tlab_alloc` result. None of the
three existing reports could distinguish them, because the message
printed neither the site nor the value.

The diagnostic is in place but has **not yet caught a firing**, so the
question that motivated it is still open.

## What's not been attempted

No kill-switch A/B (e.g. disabling G1's parallel evacuation) was run — this
investigation was read-only against Azure and made no rebuild. No debug-build
repro with more diagnostics was attempted. The exact source of the misaligned
`target` reaching `make_forwarded` (bad `old_ptr` candidate vs. bad
`tlab_alloc` result) is unresolved and is the most useful next step for
whoever picks this up, followed by re-running the Jasper `compiler` package
alone, several times, to see if the ~45% crash rate reproduces in isolation
(which would strengthen the workload-correlation reading) or was itself an
artifact of this one run's host load/timing.
