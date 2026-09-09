# `TestBnf`'s missing completion token was H2's 100 ms `Sentence` budget, not a rule bug — RETIRED 2026-09-08

**Retires `docs/known-issues/h2/bnf-completion-omits-the-user-defined-function-token-20260908.md`,
opened and closed the same day.** Its hypothesis is refuted below; its finding is
a re-discovery of a mechanism this repository already owns, on the same test, at
the same assertion. The live page is
`docs/known-issues/h2/not-bug-h2-bnf-ruleelement-link-null-npe-autocomplete.md`,
which has carried it since 2026-07-22 and now carries this session's
measurements too.

| | |
|---|---|
| **Verdict** | Not a BNF/JDBC-metadata defect. `org.h2.bnf.Sentence` gives each statement head a **100 ms wall-clock budget**; `Bnf.getNextTokenList` catches the resulting `IllegalStateException` and returns the partial map it has. CratonVM's cold `SELECT` head costs 150-200 ms, so the `PROCEDURE` rule never runs. |
| **Still open** | Yes — as a throughput gap on the canonical page. Nothing here closes `TestBnf`. |
| **What is new** | The cold head is now **decomposed**, and 53% of it is one thing the earlier pages never named: the one-shot `java.text.Collator` bootstrap (§3). One residual the narrowing exposed — uncontended `synchronized` costing 2.1x its unsynchronised twin where HotSpot pays 1.05x — was **fixed on this branch** for the block half (§4); it does not move this page's number. |
| **Where** | Azure host 2 (`20.80.105.49`), branch `claude/h2-known-issues-retire-20260908` off `origin/dev` `2b0913374`, H2 2.4.249, JDK 25.0.4. Host load ranged 5-18 throughout; every number below is an interleaved median, never a sequential pair. |

---

## 1. The hypothesis, and why it is wrong

The retired page said:

> *"`org.h2.bnf.context.DbContextRule.addNextTokenList` for `PROCEDURE`, and how
> it matches the typed prefix against `DbProcedure.getName()`. The narrowing
> above says the input to that rule is right and its output is empty, so the
> fault is between them — a name comparison, a case fold, or an iteration over a
> collection that reads as empty."*

There is nothing between them. `probes/BnfProc.java` runs the same six lines of
H2 API and then **re-runs the walk with the budget lifted** (reflection on
`Sentence.stopAtNs`):

```text
run 0 took 153 ms -> {1#.=., 1#character=A, 1#digit=1}
run 1 took  78 ms -> {1#.=., 1#character=A, 1#digit=1, 2#CUSTOM_PRINT=INT}
run 2 took  79 ms -> {1#.=., 1#character=A, 1#digit=1, 2#CUSTOM_PRINT=INT}
--- guard disabled ---
no-guard walk: 131 heads, 72 ms -> {1#.=., 1#character=A, 1#digit=1, 2#CUSTOM_PRINT=INT}
```

The **same process** that produced a three-entry map on its first call produces
HotSpot's four-entry map on its second, and produces it on the first call too
once the timer is out of the way. `DbContextRule`, the name comparison and the
case fold are all correct; the run that read as "the rule contributed nothing"
was a run that never reached the rule.

## 2. Proved on the real class, not just the probe

Recompiling **only** `org.h2.bnf.Sentence` with a wider
`MAX_PROCESSING_TIME` and putting it first on the classpath — no VM change, no
test change:

| `MAX_PROCESSING_TIME` | `org.h2.test.unit.TestBnf` |
|---:|---|
| 100 (H2's value) | FAIL, `TestBnf.java:138` |
| 110 / 125 / 150 | FAIL, same assertion |
| **200 / 300 / 100000** | **PASS** |

So the real head costs **150-200 ms** against a 100 ms budget: the gap to close
is 1.6-2x, and it is a single number, not a family of failures.

The retired page's "How it surfaced" row is also wrong. It said `TestBnf` "did
not fail before 2026-09-07 because the class aborted earlier". The canonical
page records this exact assertion failing on **2026-08-10** with
`SELECT CUSTOM_PR` measured at 230 ms and the `INT` token absent — before the
crash fix it credits.

## 3. What the cold head is actually made of — the new finding

`Bnf.getNextTokenList` calls `sentence.start()` **per head**, so the budget is
per statement, and only one statement matters. `probes/BnfHeads.java` times each:

```text
HotSpot   head 1 'SELECT' 28 ms;  walk: 131 heads, 35 ms, VERDICT PASS
CratonVM  head 1 'SELECT' TRIPPED THE 100ms BUDGET after 158 ms; VERDICT FAIL
```

Head 1 is 80% of HotSpot's whole walk and 100% of CratonVM's problem. The other
130 heads cost 7 ms on HotSpot.

`probes/BnfSplit.java` splits head 1 in two by doing one
`Collator.getInstance()` **outside** the timed region. Interleaved, N=7, medians,
JIT on:

| | cold head 1 | = `java.text.Collator` bootstrap | + rule walk |
|---|---:|---:|---:|
| HotSpot 25.0.4 | 22 ms | 18 | 6 |
| CratonVM | **126 ms** | **67** | **51** |
| ratio | 5.7x | 3.7x | 8.5x |

**53% of the failing head is one `Collator.getInstance()`.** H2 reaches it from
`org.h2.util.StringUtils.startsWithIgnoringCase`, which builds a collator on
*every* call:

```java
public static boolean startsWithIgnoringCase(String text, String prefix) {
    if (text.length() < prefix.length()) return false;
    Collator collator = Collator.getInstance();
    collator.setStrength(Collator.PRIMARY);
    return collator.equals(text.substring(0, prefix.length()), prefix);
}
```

`DbContextRule.autoComplete` calls it once per candidate, so the very first one
inside the timed walk pays for `RuleBasedCollator`'s whole table build:
`RBTableBuilder.addComposedChars`, `sun.text.UCompactIntArray.initPlane`,
`PatternEntry$Parser`, and the `jdk.internal.icu` normaliser — the top of the
`--stack-sample-ms 2` profile of the real `TestBnf` walk, 141 of its samples.

Two candidate explanations for the bootstrap gap were checked and **ruled out**:

* **A different collator.** The default locale does differ (`en` on HotSpot,
  `en_US` on CratonVM, `probes/LocaleCheck.java`) but the rules are
  byte-identical — length 850, same `hashCode` — so both VMs build the same
  table.
* **A broken `Collator.getInstance` cache.** `probes/CollatorCache.java`:
  `200 x getInstance(Locale.US)` is 3 ms on CratonVM against 7 ms on HotSpot.
  The per-locale cache works; only the first build is slow.

## 4. The residual, sized: uncontended `synchronized` is 2.1x where HotSpot is 1.05x

The other 47% — the rule walk — is dominated by `RuleBasedCollator.compare`
(`collator.equals` inside `startsWithIgnoringCase`). `probes/CollatorSplit.java`,
2000 iterations:

| | `getInstance` | `setStrength` | `equals` | `substring` |
|---|---:|---:|---:|---:|
| HotSpot `-Xint` | 0 ms | 0 | **37** | 0 |
| CratonVM `--nojit` | 15 | 0 | **446** | 2 |

`equals` is **12x** HotSpot's interpreter where the ambient interpreter ratio on
this host is ~7x, so something in `compare` is disproportionate. A
`--stack-sample-ms 1` profile of that loop puts `java.lang.StringBuffer.charAt`
and `.length` at the top — `synchronized` methods, called per character by the
ICU normaliser through `NormalizerBase.nextNormalize`.

`probes/SyncCost.java` prices that directly. Ratios *within one process*, so
host load cancels:

| | plain call | `synchronized` method | `synchronized` block | `StringBuilder.charAt` | `StringBuffer.charAt` |
|---|---:|---:|---:|---:|---:|
| HotSpot `-Xint`, 1M | 23 ms | 30 (**1.30x**) | 23 | 120 | 126 (**1.05x**) |
| CratonVM `--nojit`, 1M | 155 | 494 (**3.19x**) | 321 | 598 | 1283 (**2.15x**) |

**An uncontended `synchronized` JDK method costs CratonVM 2.1x its unsynchronised
twin; HotSpot pays 5%.** Where it goes, from the code: every uncontended
`monitorenter` runs `ThreadRegistry::complete_jmx_monitor_enter` (an `RwLock`
read, a `FxHashMap` lookup, three `parking_lot` mutexes and a linear membership
scan), every `monitorexit` runs `remove_jmx_locked_monitor` (the same lookup plus
a linear `retain`), the `monitorenter`/`monitorexit` opcode handlers clone three
`Arc`s each to pre-build a JEP 358 message only a null operand can need, and the
`ACC_SYNCHRONIZED` invoke path copies the argument vector before it knows whether
the acquire will block. A throwaway build with only
`complete_jmx_monitor_enter` / `remove_jmx_locked_monitor` stubbed out took
`StringBuffer.charAt` from 1501 ms to 1124 ms per million — a quarter of the
overhead in two functions.

**PARTLY FIXED, 2026-09-08, and it did not move this page's number.** The
bookkeeping named above is now off the uncontended path: the two opcode
prologues take a peek-first fast path (`monitor_operand_slow` is `#[cold]` and
runs only for a null / `Uninitialized` / smuggled-`long` operand), and the JMX
slots moved into a `JmxMonitorBook` behind an `Arc` that the owning thread
caches, so neither acquire nor release consults the registry map and the
uncontended arm skips the two contended slots entirely. `CRATONVM_MONITOR_FASTPATH=0`
restores the old path in the same binary, which is how the arms below were taken
(N=9, interleaved, medians of per-run ratios):

| arm | `syncMethod/plain` | `syncBlock/plain` | `SBuffer/SBuilder` |
|---|---:|---:|---:|
| HotSpot `-Xint` | 1.36 | 1.04 | 1.02 |
| CratonVM, fast path **off** | 3.18 | 2.13 | 1.92 |
| CratonVM, fast path **on** | 2.96 | **1.45** | 1.91 |

A `synchronized` block's excess over its unsynchronised twin falls from 113% to
45%; a `synchronized` *method*'s from 218% to 196%. The block half is done; the
method half is not, and the profile says why — what is left is not monitor work
at all. In the `synchronized`-method loop the JMX pair fell from 8.9% to 3.2% of
samples, and the top of what remains is the INVOKE path taking a slower route for
an `ACC_SYNCHRONIZED` callee than for a plain one: `ProfileStore::get_or_insert_borrowed`
+ `record_receiver_borrowed` (6.7%) and `core::hash::sip::Hasher` (4.6%), where
the same loop with a plain callee uses the memoized `record_receiver_memoized`
and shows no SipHash at all. That is a receiver-profile memoization miss on the
synchronized invoke path, and it is the next thing to pull — in its own lane.

**And it does not move the cold head.** Re-measured on the fixed binary:
174 ms against HotSpot's 35 on a host busy enough to have moved HotSpot itself
from 22 (ratio 5.0 against 5.7). The head is bootstrap-dominated, and
`StringBuffer.charAt` — the synchronized method the collator path actually calls
— is in the *method* half, which barely moved. This is exactly why the two were
measured on separate oracles.

**The unfixed remainder, for the next lane:** the synchronized-invoke receiver-profile
miss above, and `probes/SyncCost.java` as the oracle. The ratio columns are the
acceptance criterion and they do not depend on what else the host is running;
`probes/JmxMonitorOwnership.java` is the correctness oracle that has to stay
green on both settings of the switch.

## 5. Levers checked and found inert, so nobody re-runs them

* **JIT tier-up threshold.** `CRATONVM_JIT_THRESHOLD` 1 / 4 / 10 / 50 and
  `CRATONVM_JIT_C2_FIRST_CALL=1` all land inside the run-to-run spread. Confirms
  the canonical page's 2026-08-10 result on the same class.
* **`-Xverify:none`.** Makes the collator bootstrap **twice as slow** (medians
  252-291 ms against 113-141 ms). Turning verification off is not a lever here;
  it is a separate anomaly worth its own look, since it says the interpreter
  depends on `verified_code` for speed and falls off a cliff without it.
* **The monitor bookkeeping alone.** Both the throwaway stubbed build and the
  real fix that followed it moved `probes/BnfHeads.java` not at all — the head is
  bootstrap-dominated, and §4's fix has to be judged on its own oracle, not on
  this one.

## 6. Reproducing

```bash
# the differential, six lines of H2 API plus a guard-lifted re-run
javac -cp "$H2CP" probes/BnfProc.java -d /tmp/p
cratonvm --java-home "$JDK" --nojit -c "/tmp/p:$H2CP" BnfProc

# per-head cost against the 100 ms budget
javac -cp "$H2CP" probes/BnfHeads.java -d /tmp/p
cratonvm --java-home "$JDK" -c "/tmp/p:$H2CP" BnfHeads

# the split that names the collator bootstrap (add `preheat` to move it out)
javac -cp "$H2CP" probes/BnfSplit.java -d /tmp/p
cratonvm --java-home "$JDK" -c "/tmp/p:$H2CP" BnfSplit preheat

# the residual's oracle
javac probes/SyncCost.java -d /tmp/p
cratonvm --java-home "$JDK" --nojit -c /tmp/p SyncCost 1000000
```

Every number in §3 is a median of 7, with the two VMs alternating inside one
loop. On this host that is not optional: the load average moved between 5 and 18
during this session, and a sequential pair of arms measured 130 ms and 226 ms for
the same command twenty minutes apart.
