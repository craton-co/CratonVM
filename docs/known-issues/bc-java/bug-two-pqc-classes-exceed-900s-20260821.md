# The two `pqc` classes that time out are slow, not wedged

## What was measured

The full 53-class bc-java sweep (JIT on, `--Xmx 1g`, default collector,
`CLASS_TIMEOUT=900`) leaves exactly two classes unfinished, and they are
unfinished on **both** arms of a fix/base A/B — so nothing about them is
attributable to the change that sweep was gating:

| # | class | base | fix |
|---|---|---|---|
| 045 | `org.bouncycastle.pqc.crypto.test.AllTests` | HANG 900s rc=124 | HANG 900s rc=124 |
| 046 | `org.bouncycastle.pqc.jcajce.provider.test.AllTests` | HANG 900s rc=124 | HANG 900s rc=124 |

`rc=124` is `timeout` killing a process that was still running, so neither is a
crash. The runner records that as `HANG`, and **that label is wrong here.**

## They were still making progress when killed

A `timeout` kill cannot distinguish "wedged" from "not finished yet", which is
the whole hazard in
`a-fixture-timeout-hides-whether-it-is-slow-or-hung`. Both logs say
"not finished yet":

* Both emit a JUnit progress line (`..........E....................E.......F..E..`
  for 046) — tests are completing and their results are being written.
* Both keep producing timestamped VM output right up to the kill. 045 spans
  `03:40:09 → 03:53:50`, 046 spans `04:10:09 → 04:24:33`; the kill lands at 15
  minutes in each case. There is no quiet tail, which is what a deadlock looks
  like.

So the open question is not "where is it stuck" but "how much slower than
HotSpot is it, and does it terminate at all". A 3600 s run of each against a
HotSpot oracle on the same host is queued; this page will carry the ratio when
it lands. **Until then the honest status is: exceeds 900 s, still progressing,
completion unknown.**

## Two independent leads visible in the logs

### 1. Compiled twice — 14 times on 045

Both classes repeatedly trip:

```text
JIT compile bailed: code buffer estimate too small; retrying at the measured size
  method="…/DilithiumEngine.signSignatureInternal:([BI[B[B[B[B[B[B[B)[B"
  code_len=591 capacity=123296 wanted=140536
  method="…/xmss/BDS.initialize:([B[BLorg/…/OTSHashAddress;)V"
  code_len=772 capacity=188800 wanted=211145
```

045 pays this **14 times**, 046 twice. Each one is a full compile thrown away
and redone at the measured size. These are the largest methods in the suite —
591 and 772 bytecodes expanding to 140 KB and 211 KB of machine code — and the
estimator undershoots by 12-18% on exactly that shape. PQC code is unusually
straight-line and array-heavy, so it is the natural place for a per-bytecode
size estimate to be wrong.

Worth sizing before assuming it matters: 14 wasted compiles of ~120 KB methods
is real work, but it is not obviously 15 minutes of it. This is a lead, not a
diagnosis — see `a-flat-profile-percentage-is-a-lead-not-a-quantity`.

### 2. A root-collection gap on 046, twice

046 also logs two `cratonvm::gc::guard` ERRORs:

```text
in_published_snapshot=false  published_roots=805
last_publish_at_collection=13  collections_now=14
top_frame=org/bouncycastle/pqc/jcajce/provider/test/XMSSTest.testExhaustion pc=14

in_published_snapshot=false  published_roots=453
last_publish_at_collection=26  collections_now=27
holder=frame#17 …/XMSSTest.testKeyExtraction pc=48 local[3] kind=0 live=true
```

The guard's own wording is that this is "a root COLLECTION gap, not a mark or
sweep one" — the snapshot the collector marks this thread from did not contain a
slot the thread's frames hold. Note `last_publish_at_collection` is exactly one
behind `collections_now` in both instances: the snapshot is **one collection
stale**, and the second names the missing holder precisely
(`local[3]`, `live=true`).

This is the same family as
`bug-g1-evacuates-live-jit-reference-20260819.md` and
`bug-oop-map-coverage-bit-is-presence-not-completeness-20260820.md` — a live
reference the collector's root set does not contain — but it is a *different*
mechanism (a stale published snapshot, not a skipped conservative scan), it
fires on the default collector, and it is unaffected by those fixes: the counts
are identical on both arms of the A/B. It deserves its own investigation and
should not be folded into either record.

## What this page does not claim

* **Not that they never finish.** Nothing here has run them to completion. The
  900 s bound is the harness's, not a property of the workload.
* **Not that the re-compiles are the cost.** The count is measured; its share of
  the runtime is not.
* **Not that the two classes share a cause.** 045 shows 14 re-compiles and zero
  guard errors; 046 shows 2 re-compiles and 2 guard errors. They are grouped
  here because they share a symptom and a harness bound, which is the weakest
  possible reason to group two things.

## Repro

```bash
cd /data/cratonvm/apps/bc-java
cratonvm --java-home /data/toolchain/jdk-25 --Xmx 1g \
    -Dbc.test.data.home=/data/cratonvm/apps/bc-test-data \
    -Dtest.java.version.prefix=25 \
    -c "$(cat /data/bcjca-classpath.txt)" \
    junit.textui.TestRunner org.bouncycastle.pqc.crypto.test.AllTests
```

Swap the class for `org.bouncycastle.pqc.jcajce.provider.test.AllTests` for the
second. Add `-cp` and `$JAVA_HOME/bin/java` for the HotSpot oracle.
