# The five owed records, finally run — and none of the three probes comes back clean

**Status: MEASURED 2026-09-02.** The `unverified_records` ratchet holds five
records as OWED: `W7-24`, `W7-57`, `W7-58`, `W7-70`, `W7-81`, all "fixed in
source" on 2026-08-11/12 by lanes that could not run Rust. Three probes cover
all five. This is what happened when they were run.

It is **not** a verdict on those records. It is the run they asked for, plus the
row counts, so their owners can adjudicate row by row against their own
expectation tables — which is work this page deliberately does not do.


> **VERIFIED AGAINST A BINARY 2026-09-02, and superseded on 2026-09-03: all
> five are now closed.** This page said *"Nothing is closed. The five stay OWED
> in `types/tests/unverified_records.rs`"*, and deliberately did not adjudicate
> them — *"work this page deliberately does not do"*. That work has since been
> done, record by record, against each one's own expectation table:
>
> ```text
> W7-24   discharged 2026-09-02   the door defect was real and the fix was right
> W7-57   discharged 2026-09-03   sweep holds; filterOutFlushFailureWins still red
> W7-58   discharged 2026-09-03   bb_state's direct-buffer arm confirmed live
> W7-70   discharged 2026-09-03   fixed on both shipping arms, LIVE on --synthetic-jdk
> W7-81   discharged 2026-09-03   both echo tokens read the "after" column
> ```
>
> **The arm lesson at the top of this page paid off twice more.** It says a
> record that tells you which arm to use is telling you its result is
> arm-specific. Re-running the same three probes on all four arms is what
> separated W7-70's live byte loss (`--synthetic-jdk` only) from its repaired
> shipping arms, and what showed W7-58's `getIntLE`/`putIntLE` residuals to be
> `--synthetic-jdk`-only — neither of which is visible from one arm.
>
> **Its row counts have also been superseded, and by more than drift.** This
> page reported W7-57's probe at *129 rows vs HotSpot 120, 35 differing*. The
> same probe on the same tree today gives 13 differing under `--synthetic-jdk`
> and 6 on both shipping arms. Most of that is merged `dev` work between the two
> dates, not a mode effect — which is the standing caution that a row count is
> only a result against a named binary.
>
> **Why this page was on the ratchet at all, which is worth one line.** It is a
> MEASURED page and always was. It matched an unverified marker because its
> third line names the file `types/tests/unverified_records.rs`, and the matcher
> tests lowercase substrings — so the identifier `unverified_records` reads as
> the word "unverified". The marker list was deliberately widened to substrings
> after it saw 5 records out of a real population of 61; narrowing it again to
> dodge one filename would trade a false positive for the blindness that caused
> the original defect. The false positive is the cheaper of the two, and this
> note is the correct way to clear it.
## First, the arm. I got it wrong once and the records had already said so

The obvious run — a default build, compatible mode and `--jdk-only` — tests the
wrong code for all three probes:

* `W7-58` §7 is headed **"Where this is reachable — read this before running the
  probe"** and says `register_nio_natives` runs *"only in a `--features
  synthetic-jdk` binary running `--synthetic-jdk`"*, and that a real
  `DirectByteBuffer` *"can no longer reach any of them"*.
* `W7-57` says its sites are *"registered only under `--synthetic-jdk`; in
  Compatible mode the real bytecode"* runs instead.
* `W7-24` carries its sites as *"not reachable in a default run"*.

I ran the default build first anyway. Those numbers measured the real path, not
the fixed one, and said nothing about these records either way. **A record that
tells you which arm to use is telling you its result is arm-specific; running
the other arm produces a number that looks like evidence and is not.**

Everything below is a `--features synthetic-jdk` debug binary run with
`--synthetic-jdk`, against HotSpot 25.0.4+7.

## The runs

```text
probe                        HotSpot   synthetic-jdk   differing
CloseFlushSwallowProbe       120 rows      129 rows       35
DirectByteBufferStateProbe   285 rows      264 rows       39
HttpServerWildcardAddress      7 rows        7 rows       (see below)
```

**Read the ROW COUNTS before the diffs.** `DirectByteBufferStateProbe` emits 21
FEWER rows than HotSpot: those rows are UNTESTED, not passing, and a diff count
alone would have hidden that. `CloseFlushSwallowProbe` emits 9 MORE, which is a
different shape again.

### Some of these are documented residuals — W7-58 predicted them

W7-58's own table says of the little-endian family: *"`*.getIntLE`,
`*.putIntLE.*` — still red — the hard-coded `to_be_bytes` / `from_be_bytes`
family"*. The run agrees:

```text
BAD  heap.getIntLE = 472066609   (expected 824845084)
BAD  heap.putIntLE.byte4 = 1     (expected 4)
```

That is the record being RIGHT about what it did not fix, and it should be
scored as such rather than as a failure.

### Others are not obviously documented, and one is an internal error

```text
zipOutClosePropagatesError
  HotSpot        java.lang.Error: zip-close-boom
  CratonVM       java.lang.NullPointerException:
                   Cannot invoke "java.util.HashSet.add(Object)" because "this.names" is null
```

HotSpot propagates the test's own error; this VM raises an internal NPE from a
null `names` set. A caller cannot distinguish "your close handler threw" from
"the zip stream is broken inside", which is the swallow-vs-propagate axis
`W7-57` exists for.

```text
filterOutFlushFailureWins   HotSpot: java.lang.Error: flush-boom   CratonVM: none
```

`W7-57`'s table lists the HotSpot value as expected; the run does not produce it.

### The HttpServer arm needs its probe fixed before it can be scored

```text
HttpServer getAddress()   HotSpot /[0:0:0:0:0:0:0:0]:33483
                          CratonVM 0.0.0.0/0.0.0.0:37489
```

The wildcard family differs — IPv6 versus IPv4 — which is this record's subject.
But the probe also prints **ephemeral port numbers**, which differ on every run
of either VM, so a raw diff of its 7 rows counts noise as signal. The port must
be normalised before this probe can produce a number anyone should quote.

## What this changes

Nothing is closed. The five stay OWED in `types/tests/unverified_records.rs`,
with their entries updated to say they have now been RUN and on which arm, so
the next reader starts from a measurement instead of from zero. Row-by-row
adjudication against each record's own expectation table is the owning lane's
call: this page cannot tell a documented residual from a regression for 74
differing lines, and guessing would be worse than the silence it replaced.

## Reproduce

```bash
cargo build -p cratonvm-cli --features synthetic-jdk        # the arm matters
for p in HttpServerWildcardAddressProbe CloseFlushSwallowProbe DirectByteBufferStateProbe; do
  git show 3b2901531^:probes/$p.java > probes/$p.java       # deleted by 3b2901531
done
javac -d /tmp/p probes/*Probe.java
(cd /tmp/p && $JDK/bin/java -cp . <P>)                              > hs.txt
(cd /tmp/p && cratonvm --java-home $JDK --synthetic-jdk -cp . <P>)  > syn.txt
diff hs.txt syn.txt
```
