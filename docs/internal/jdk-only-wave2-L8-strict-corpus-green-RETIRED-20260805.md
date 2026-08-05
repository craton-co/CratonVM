# L8 — Criterion 6: strict corpus green — RETIRED 2026-08-05

**Was:** `docs/feature-designs/jdk-only-wave2/L8-strict-corpus-green.md`
**Owned:** `probes/`, `regression-suite/`, `scripts/` — "no production source"
(that boundary was crossed four times; each crossing is named below).

## What the lane claimed, and whether it held

> Criterion 6 is not a formality to tick once the list is done. It is where the
> defects are.

It held, and by a wider margin than the doc expected. One new probe found
**four divergences on its first run**, and chasing them found **three more**
underneath — because the first two were binding failures that had been hiding
everything behind them.

Seven defects. Five fixed, two filed. Plus the lane's own open residual
diagnosed and fixed, and two bugs found **in the instruments**.

## The four steps

### 1. Widen the corpus — H2 done, three suites not

`probes/JdkOnlyPlatformProbe` covers what step 4 asked for. On the suites:
H2's first 60 classes were run on three arms —
[the triage record](../known-issues/h2/h2-under-jdk-only-three-arm-triage-20260805.md).

| arm | PASS | FAIL | HANG |
|---|---:|---:|---:|
| HotSpot 25 | 58 | 2 | 0 |
| `--jdk-only` | 46 | 6 | 8 |
| `--real-jdk` | 47 | 5 | 8 |

Twelve classes CratonVM fails that HotSpot passes, and **only three are
strict-only**: `TestDuplicateKeyUpdate` (deepest frame
`StringLatin1.toUpperCase`, reached from ordinary SQL tokenization),
`TestFullText` (Lucene's mmap `IndexInput`), `TestRights`. Reporting
strict-vs-HotSpot alone would have filed twelve strict-mode defects, nine
wrongly — which is the whole reason the protocol demands both modes.

Hibernate, Spring Boot and Tomcat were **not** run. They need the same
three-arm pass on a quiet host; `scripts/cratonvm-prefix-args.sh` drives all
three runners through their existing binary-path variables (`CRATONVM_EXE`,
`CV_BIN`) with no runner edit, which is how H2 was driven.

### 2. Censuses from a real workload — done, and the probes are NOT redundant

`scripts/jdk-only-census.sh` already took `PROBE_CP` / `PROBE_CLASS`; nobody
had pointed it at a suite. Same instrument, same binary, three H2 classes
against the three standing probes:

| workload | dispatched slots |
|---|---:|
| `JdkOnlyCensusLoadProbe` | 406 |
| `JdkOnlyBreadthProbe` | 431 |
| `JdkOnlyIcHotProbe` | 52 |
| **probe union** | **674 / 183 classes** |
| `TestSpatial` | 653 |
| `TestCompatibility` | 680 |
| `TestSequence` | 575 |
| **H2 union** | **729 / 187 classes** |
| **both** | **979** of 11,526 registered |

The doc's "401 of 11,909" was low on both terms: the probes reach 674, and the
registry is 11,526 now that the `String` natives have been dropped.

**The finding that matters: 305 slots only the suite reached, and 250 only the
probes reached.** Neither is a superset. A census taken from suites alone would
lose a quarter of what is currently measured, so "take the censuses from the
suites, not the probes" is wrong as stated — take the union. L6 and L7 should
read `census-union.json`, not either half.

### 3. Probes CI-runnable — done

`scripts/jdk-only-strict-probes.sh` runs three arms on one image and one set of
class files and diffs both CratonVM transcripts against HotSpot. It separates
**exit 4** (an arm did not complete) from **exit 5** (a transcript diverged),
because a `timeout` kill prints a truncated transcript that reads exactly like a
clean short run — the specific way the first strict census runs lied. It names a
both-modes divergence as a compatibility defect rather than a strict one, and
reports how many VM/launcher lines its filter dropped per arm so a normalizer
that starts eating output shows up as a number.

Wired into `ci.yml`'s `jdk-only` job on both OSes and both JDKs, transcripts
uploaded. `continue-on-error` is on the **step**, not the job, and the comment
names the two records that must close before that line comes off.

### 4. The five uncovered surfaces — done

`probes/JdkOnlyPlatformProbe` covers ProcessBuilder, security providers,
virtual threads, agents/attach and JNI, with two fixtures a
determinism-bound suite cannot ship, both built at run time rather than
committed:

* `probes/jdkonly_jni_probe.c` — eight families, including one bound by
  `JNI_OnLoad`/`RegisterNatives` whose C symbol is deliberately unrelated to
  its Java name, so a VM that only resolves by mangled symbol fails that one
  and nothing else. This closes `regression-suite/jdk-only-coverage.txt`'s own
  limitation 1, which said the fixture had to exist outside the suite.
* `probes/JdkOnlyProbeAgent.java` — a real `-javaagent:` jar whose premain
  registers an observing transformer.

Virtual threads were covered by **nothing** before this: no `RJdk*` vector
names `ofVirtual`, `startVirtualThread` or `isVirtual`.

## What it found

### Fixed

| defect | where |
|---|---|
| `ProcessBuilder.redirect{Input,Output,Error}(File)` were stubs. Output/error returned the receiver and **dropped the file** — the getter still answered `PIPE`. Input wrote the `File` into raw slot 3, which on a real `ProcessBuilder` is the `redirectErrorStream` **boolean**, and the redirect still never happened, so a child reading that pipe never saw EOF and the parent's `readAllBytes()` **blocked forever**. | `native-builtins/src/phases_late.rs` — deleted; the real bytecode already worked, as `Redirect.appendTo`/`INHERIT` proved in the same run |
| `SunMSCAPI` seeded unconditionally. It wraps the Windows CryptoAPI and ships only in the Windows JDK, so Linux answered thirteen providers where HotSpot answers twelve, and `getProvider("SunMSCAPI")` returned a live 16-service Provider for a class that raises `ClassNotFoundException`. | `native-builtins/src/jca/provider_chain.rs` |
| **JNI mangling never escaped `$`.** `jni_encode`'s catch-all was gated on `is_ascii()`, so `$` — the separator in every nested class's binary name — passed through verbatim. **No native on a nested class could bind**, which is most of them. | `vm/src/native/jni.rs` |
| **`RegisterNatives` decoded `JClass` as an object handle** while `FindClass` returns a raw `ClassId` and the rest of the table decodes it as one. It always returned `JNI_ERR`, so the `FindClass`+`RegisterNatives` idiom every `JNI_OnLoad` is built on registered nothing. | `vm/src/native/jni.rs` |
| **`Net.poll` held the socket-map read lock across the listener park** — the lane's own open residual, below. | `native-io/src/net.rs` |

### Filed

| defect | record |
|---|---|
| `ConcurrentHashMap.newKeySet()` returns a plain `HashSet`, so concurrent churn leaves an empty table reporting `size=18`, `ThreadPerTaskExecutor` never reaches `TERMINATED`, and `ExecutorService.close()` never returns | [record](../known-issues/vm/concurrenthashmap-newkeyset-returns-a-plain-hashset-20260805.md) |
| JNI argument/return marshalling: `jint`/`jlong`/`jdouble` returns come back `0`, array commit-back dropped, object-array reads `null`, `SetIntField` lost, upcall returns the C code's null-method-id sentinel, `ThrowNew` delivered a call late | [record](../known-issues/vm/jni-argument-and-return-marshalling-is-wrong-20260805.md) |
| `Instrumentation.addTransformer` accepts a transformer that is never called, while `isRetransformClassesSupported()` answers `true`; `VirtualMachine.list()` throws `InternalError` | [record](../known-issues/vm/java-agent-transformer-never-fires-and-attach-list-throws-20260805.md) |
| `KeyStore.setEntry` with a `SecretKeyEntry` is a silent no-op on PKCS12; `store()` then writes a valid 32-byte empty keystore without throwing | [record](../known-issues/vm/pkcs12-setentry-secretkeyentry-is-a-silent-noop-20260805.md) |
| H2's three strict-only classes | [record](../known-issues/h2/h2-under-jdk-only-three-arm-triage-20260805.md) |

## The residual this lane opened, closed

[Bounded socket operations hang about one run in five](../known-issues/bounded-socket-operations-hang-about-one-run-in-five.md)
named five candidate call sites and asked for a thread dump as the cheapest
next step. It was right. Twenty-five runs with `--stack-dump-on-timeout=45`
inside `timeout 90` hung **6 times** — the recorded rate — and every hung run
dumped frames instead of dying silently. **All six dumps identical**: accept
thread last in `Net.poll`, main thread last in `Net.socket0`.

That ruled out four of the five candidates and named a thread the record never
considered. `net_poll` wrote `return net_poll_listener(...)` from inside a read
guard's scope; Rust evaluates the call before unwinding the scope, so the whole
park ran holding the guard, and the park's loop re-acquires the same
writer-preferring `RwLock` every slice. A concurrent `Net.socket0` queues
between the two reads and nothing moves. One flag turned "the run stops after
the `nio` line" into two named frames in under a minute.

**A/B: pre-fix 13/60 hung, post-fix 0/60** — and the first A/B proved nothing.
Thirty sequential runs per arm on a quiet host gave 0/30 on *both*, pre-fix
included: a failed reproduction, not a passing test. The race needs contention,
so the second attempt generated 10 concurrent probes per wave and alternated
the arms **within** each wave, because running one arm to completion and then
the other compares two different machines. The pre-fix arm hung in all six
waves; the post-fix arm never did.

## Two bugs in the instruments

Worth more than they look, because both were invisible to the probes' own
authors and one was a documented claim:

* `JdkOnlyCensusLoadProbe` printed the **ephemeral port** its `net` section
  binds. It therefore diverged from HotSpot on every single run, while the lane
  doc described it as byte-identical. The gate's first catch was the
  instrument, not the VM.
* `JdkOnlyPlatformProbe` used try-with-resources on an executor — an unbounded
  `ExecutorService.close()` — breaking the lane's own first rule. It hung for
  the full bound and took the `agent` and `jni` sections with it, so a real
  defect arrived as a truncated transcript. Explicit `shutdown()` plus a bounded
  `awaitTermination` turns it into `terminated=false` against HotSpot's `true`.
  The `jni` section likewise accumulates into a builder and prints in a
  `finally`, which is the only reason the `$`-mangling fix's effect was
  visible: the failure moved from the first family to the last.

## Ownership crossings

The lane owned no production source. Four fixes crossed that line:
`phases_late.rs` and `jca/provider_chain.rs` (unowned by any wave-2 lane),
`vm/src/native/jni.rs` (unowned), and `native-io/src/net.rs` (**L5's**, one
function, no `register_with_kind` call site touched). The two defects in
`native-collections/src/lib.rs` (**L10's**) were filed, not fixed.

## Done when — against the doc's own bar

> The suites run under `--jdk-only` with their failures either fixed or filed,
> the probes are in CI, and the censuses driving L5/L6/L7 come from a real
> workload.

* Suites: **H2 only.** Three arms, triaged, three strict-only regressions
  named. Hibernate/Spring Boot/Tomcat are not run, and the exact mechanism for
  running them is recorded rather than left to be re-derived.
* Probes in CI: **yes**, advisory at the step level with the promotion
  condition stated.
* Censuses from a real workload: **yes**, and with the correction that the
  union is the right input, not the suite alone.

The lane retires with its instrument permanent and its thesis confirmed: of the
seven defects here, exactly **zero** were introduced by `--jdk-only`. Every one
was already wrong in Compatible mode and had simply never been executed.
