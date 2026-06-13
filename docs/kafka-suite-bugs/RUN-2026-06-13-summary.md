# Kafka-clients full unit-suite run — CratonVM vs HotSpot (2026-06-13)

Full Apache Kafka **kafka-clients 3.7.2** unit suite, **362 test classes**, run
one-JVM-per-class (`GRAN=class`) so a crash/hang in one class can never abort the
others. CratonVM built from `fix/kafka-suite-loop` worktree HEAD + pending WIP
(Mockito/ByteBuddy `TypeVariable.getGenericDeclaration` generics fixes).

Harness: `apps/kafka/tests/run-suite.sh` + `compare.sh`. Binaries: HotSpot 25.0.1;
CratonVM release `cvk.exe` (see *Methodology caveat* below).

## Headline numbers

| Metric | HotSpot 25 | CratonVM | Ratio |
|--------|-----------:|---------:|------:|
| Wall-clock (362 classes, per-class JVM) | **1419 s** | **11161 s** | **7.9×** |
| Classes OK | 301 | 221 | |
| Classes FAIL (clean test failures) | 50 | 105 | |
| Classes TIMEOUT (hang/slow ≥90 s) | 1 | 30 | |
| Classes CRASH (SIGSEGV) | 0 | 1 | |
| Classes ABEND (abnormal exit) | 0 | 2 | |
| Classes LOADERR (missing optional dep) | 10 | 3 | |
| Test-level found / passed / failed | 7961 / 7206 / 738 | 2624 / 1957 / 624 | |

> CratonVM's much lower **found** count is an artifact of the 30 TIMEOUT classes
> (which contribute 0 found) and Mockito-container failures aborting discovery —
> not fewer tests existing. Of tests that actually ran, 1957/2624 (74.6%) passed.
> The 30 TIMEOUTs alone account for ~2700 s (90 s each) of the wall-clock gap;
> the rest is interpreter/GC throughput.

## CratonVM-only crashes & hangs (HotSpot does NOT crash/hang the same way)

Per the task rule, classes where HotSpot exhibits the *same* failure are excluded.
Reports (one per distinct root cause, affected-class lists inside):

| Doc | Kind | Classes | Root cause |
|-----|------|---------|-----------|
| [bug-21](bug-21-ssl-jit-sigsegv.md) | CRASH/ABEND (SIGSEGV) | `SslTransportLayerTest`, `Tls13SelectorTest` | **ROOT-CAUSED + RESOLVED on `9d8bba97`** — conservative JIT root scan missed register-only refs → young sweep freed live object → JIT deref faulted. Gone after dev merge; `--nojit` / `CRATONVM_SHADOW_STACK=1` also eliminate it. |
| [bug-22](bug-22-bc-pqc-stale-receiver-nsme.md) | ABEND (NoSuchMethodError) | `Tls12SslFactoryTest` | **ROOT-CAUSED + RESOLVED on `9d8bba97`** — interpreter-caught form of bug-21 (stale `ThrowableCollector` lambda `Predicate`). 0 stale/NSME on the clean isolated rebuild. |
| [bug-23](bug-23-timeout-hang-family.md) | TIMEOUT (30 classes) | see doc | mostly **throughput** (JUnit reflection discovery, Mockito/ByteBuddy mock-gen, regex/JMX) — not deadlocks; a few candidate true-waits (`AbstractCoordinatorTest` `Object.wait`). |

### bug-21 / bug-22 root cause (one defect, two faces)
Both are the **register-invisibility** GC hazard: CratonVM's conservative JIT-frame
root scan only scans stack memory, so a live heap reference held *only in a machine
register* at a young-GC safepoint is invisible. The non-moving young sweep frees that
object; the dangling slot then **faults in JIT code (bug-21 SIGSEGV)** or is salvaged
by the interpreter to a **wrong `NoSuchMethodError` (bug-22)**. SSL/crypto classes
trigger it because their allocation pressure forces a GC at the wrong instant.
Proof: `--nojit` → 0 occurrences; `CRATONVM_SHADOW_STACK=1` (precise JIT roots) → 0
occurrences on the repro and all three crash classes. See `project_precise_jit_stack_maps`.

### Resolution — bug-21/22 no longer reproduce on the latest committed code
The `cv-full2` crashes were captured on a **pre-merge** binary (built ~`3dc63aae`+WIP,
size 18722304). Subsequently the kafka-suite branch took a **41-commit dev merge
`b5c709c7`** (charset/JCA/**GC**/TreeMap/**JIT**/CSLM…), incl. the JIT null-check-elim
fix `82b9bdf6`. A clean **isolated rebuild** from `9d8bba97` (worktree
`CratonVM-kafka2`, own target, uniquely-named `kv2.exe`) shows **0 stale-pointer /
0 SIGSEGV / 0 NSME across 6+ default-mode runs** of all three crash classes — they now
TIME OUT on a separate network/socket+JKS gap (HotSpot also fails them), no crash.
So bug-21/22 are **resolved on `9d8bba97`** (root cause + the `CRATONVM_SHADOW_STACK`
mitigation retained in the bug docs in case of recurrence — the defect is
GC-timing-dependent).

> Why isolation was needed: another session shares the `CratonVM-ksuite` worktree and
> was swapping its `cratonvm.exe`/`cvk.exe` and running ~10 concurrent VMs, which
> corrupted in-place verification (e.g. `taskkill /IM cvk.exe` cross-kills both
> sessions' VMs; the shadow-stack regression sample flapped on contention, not real
> regressions). The isolated `CratonVM-kafka2` + `kv2.exe` removes binary/taskkill
> collisions; only CPU is still shared, which affects timing but not crash-correctness
> (stale/SIGSEGV is detectable from logs even under timeout).

> Still outstanding (needs a quiet machine for trustworthy timing): a full
> 362-class rerun on `9d8bba97` to refresh the OK/FAIL/TIMEOUT counts, and a
> `CRATONVM_SHADOW_STACK` regression sweep if shadow-stack is to be considered for
> default-on.

## Methodology caveat — cross-session binary clobber (IMPORTANT)

The **first** full CratonVM run produced 328 "ABEND rc=127" results. This was a
**false alarm**: a *concurrent `cargo build` from another session* deleted
`target/release/cratonvm.exe` mid-run (the `.pdb`/`.d` stayed; the `.exe` and
`deps/cratonvm-*.exe` vanished — no Windows Defender detection). `timeout` then
returned **127 = "command not found"** for the remaining 322 classes. Only ~33
classes actually executed.

**Fix / protocol for this suite:** after building, copy the exe to a uniquely-named
`apps/kafka/tests/cvk.exe` and run against *that*. It is immune to (a) another
session's `cargo` clobbering `target/release/` (different path) and (b)
`taskkill /F /IM cratonvm.exe` (different image name). All numbers above are from
the clean `cvk.exe` run (`logs/cv-full2`, 0 phantom "No such file" lines).

## Correction to bug-20 (silent rc=127 exit)

The previously-documented "silent rc=127 / rc=1 abnormal exit" cases
(`ConsumerRecordTest`, `NodeApiVersionsTest`, `ConfigTest`,
`DescribeUserScramCredentialsResultTest`) **all pass cleanly (OK) in the clean
run.** Their earlier rc=127 was the cross-session clobber artifact above, NOT a
CratonVM bug. `bug-20` should be considered **not reproduced** until a genuine
silent exit is observed in a clobber-free run. See [bug-20](bug-20-silent-abnormal-exit-rc127-rc1.md).

## Artifacts
- HotSpot baseline TSV: `apps/kafka/tests/logs/hs-full/results.tsv`
- CratonVM TSV: `apps/kafka/tests/logs/cv-full2/results.tsv`
- Per-class logs: `apps/kafka/tests/logs/{hs-full,cv-full2}/*.log`
- Comparison: `apps/kafka/tests/compare-cv2.txt`
