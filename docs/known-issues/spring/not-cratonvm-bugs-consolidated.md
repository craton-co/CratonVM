# Spring Framework non-passing classes confirmed NOT CratonVM bugs — consolidated reference

**Purpose: stop future sessions re-investigating these.** Every entry below
was directly cross-checked against stock HotSpot JDK 25 on the same host
(Azure `20.80.105.49`), same classpath, same harness (`run-suite.sh hotspot`)
— and HotSpot fails identically. None of these belong in a "CratonVM
regression" count.

## Method
The 2026-08-19 3-GC full-suite sweep (Generational/G1/ZGC, 2,848 classes)
found 88 FAIL classes common to all three collectors. On 2026-08-20, all 88
were reran individually under stock HotSpot 25 on the same Azure host, same
classpath dumps, via `run-suite.sh hotspot --list <88-class-list>`.
**Result: 87 of 88 pass cleanly on HotSpot — only 1 also fails.**

| class | why it's not a CratonVM bug | evidence | doc |
|---|---|---|---|
| `org.springframework.aot.nativex.FileNativeConfigurationWriterTests` (all 5 methods: `lambdaConfig`, `reflectionConfig`, `resourceConfig`, `jniConfig`, `serializationConfig`) | Fails identically on stock HotSpot 25, same host, same classpath — a JSON-fixture/parser issue unrelated to either VM | HotSpot: `java.lang.AssertionError: \nUnexpected: comment` on all 5 methods, `found=9 succ=3 fail=6`. CratonVM (2026-08-19 sweep, `generational-s4`): byte-identical `AssertionError: \nUnexpected: comment` on the same 5 methods. The mechanism, isolated 2026-08-20: `Unexpected: comment` is JSONAssert's `NON_EXTENSIBLE` wording for a KEY in the actual JSON that the expected JSON does not have — not a comment token in a fixture. `RuntimeHintsWriter` writes `"comment": "Spring Framework <v>"` into `reachability-metadata.json` iff `SpringVersion.getVersion()` is non-null, i.e. iff `Package.getImplementationVersion()` answers for `org.springframework.core`; on this classpath it answers on BOTH VMs (`7.1.0-SNAPSHOT`), so both write the key and both fail the test's `NON_EXTENSIBLE` comparison. Oracle: `probes/PkgVersionProbe.java` + `.expected.txt` — four rows, identical on both VMs. | the retired `bug-spring-remaining-fail-clusters-20260819` write-up, superseded by this finding |

## The other 87 classes
All 87 remaining common-FAIL classes pass cleanly on stock HotSpot 25 — they
are genuine CratonVM-specific divergences, tracked in:
* `bug-spring-concurrenthashmap-entrysetview-removeif-npe-20260819-FIXED-20260820.md`
  — the `ConcurrentHashMap.entrySet().removeIf()` NPE. **FIXED 2026-08-20**; a
  re-run of the same 90 classes scores 88 OK, the two exceptions being the class
  in the table above and the AOT/Mockito throughput wall
  (`BeanRegistrationsAotContributionTests`).
* `bug-spring-remaining-fail-clusters-20260819-FIXED-20260820.md`
  — the remaining singleton/small-cluster classes. **FIXED 2026-08-20**: an
  immutable-collection serialization defect, a `MethodHandles.collectArguments`
  adapter that reported the wrong type, and a JIT operand-spill miscompile
  between them account for every one.
* `bug-spring-methodhandle-asspreader-groovy-invocation-cluster-20260819-FIXED-20260820.md`
  — ~11 more classes, `MethodHandle.asSpreader`, confirmed **already fixed**
  on `dev` before this HotSpot baseline was even run (so they no longer
  appear in a fresh CratonVM rerun — not counted as "not a CratonVM bug",
  they were a real bug that's since been fixed).

## What this list is not
It covers only the 88 classes common to all three 2026-08-19 GC-variant
runs — not the smaller sets of classes that failed on only one or two
collectors (see the original sweep results for those). Not exhaustive over
Spring Framework's full non-passing history, only this sweep's 88.

## Repro
```bash
cd apps/spring-suite-runner
JDK25=/data/toolchain/jdk-25 SPRING=/data/cratonvm/apps/spring-framework \
./run-suite.sh hotspot --only 'FileNativeConfigurationWriterTests' --tag verify
```
