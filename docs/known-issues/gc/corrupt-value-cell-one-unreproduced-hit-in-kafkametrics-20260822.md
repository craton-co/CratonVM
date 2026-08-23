# One corrupt `Value` cell in `KafkaMetricsAutoConfigurationTests`, seen once and not reproduced

**Status: OPEN — one observation, and that is the whole of the evidence.**
**2026-08-23: the instrument now covers the doors it missed; the cell has
not recurred in a further 1975-class armed sweep. See the dated section.** Found
in a two-arm Spring Boot regression sweep on 2026-08-22 (Windows host, 1975
classes per arm). Filed because a heap-integrity guard fired and nothing else
explains it — not because it is understood.

---

## What was seen

The sweep A/B'd two binaries that differ only by
`fix/gc-known-issues-20260822` (the four collection-native fixes and the
`String[]`-rendering fix). Across 1975 classes per arm, the `cratonvm::gc::guard`
output is identical apart from ONE class in each direction:

```text
                              control          fixed
gc::guard ERROR, all kinds    35 cls / 62      35 cls / 63
  root COLLECTION gap         34 cls / 61      34 cls / 61     <- identical sets
  corrupt Value cell           1 cls /  1       1 cls /  2
     only in control          JsonMarshallerTests              <- the fixed defect, gone
     only in fixed            KafkaMetricsAutoConfigurationTests
```

The `JsonMarshallerTests` row is the defect
`corrupt-value-cell-producer-was-a-string-array-FIXED-20260822` closes, and its
disappearance is that fix working. This page is about the other row.

```text
ERROR cratonvm::gc::guard: zgc::get_field: corrupt Value cell (out-of-range
discriminant) slot=0x2c8d3d7c000
  raw0="0x0079786f72502f74"  raw1="0x000000010000000c"
```

The class PASSed (`tests=4 failed=0`) on both arms. The guard returns a benign
null, so nothing downstream noticed.

## It is NOT the defect that was just fixed

Read `raw0` as bytes, little-endian: `74 2f 50 72 6f 78 79 00` — **`"t/Proxy\0"`**.
That is UTF-8 TEXT, almost certainly the tail of a class name. `raw1` reads as
two ints, `(12, 1)`.

The `String[]` producer's signature is two HEAP POINTERS into the same arena —
an array's first two element slots decoded as a `(tag, payload)` pair. This one
is a name buffer decoded as a `Value` cell. Different shape, different producer.

## What was ruled out, and what was not

| | result |
| --- | --- |
| the class, in isolation, 6 runs per binary | **0 hits on both** |
| the class, 12-way parallel × 3 rounds, 36 runs per binary, instrument armed | **0 hits on both** |
| `CRATONVM_DBG_CORRUPT_CELL` producer record | never fired — the read does not come through `NativeContext::get_field` |

So it needs something the suite provides and 36 concurrent re-runs do not, and
it was not caught with the instrument armed.


## 2026-08-23 -- the instrument was widened, and the cell did not come back

`CRATONVM_DBG_CORRUPT_CELL` watched exactly one door when this page was
written: `NativeContext::get_field`. It now watches the INTERPRETER's own
`getfield` (where most field reads happen and which was unwatched), three more
native doors, and a safepoint BACKSTOP for a reader with no door at all -- plus
an exit census, `[corrupt-cell] decoded=N reported=M`, printed whenever the flag
is armed so that an absent line and a zero line stop looking alike.

That distinction was not academic. The FIRST armed sweep run for this chase
printed the census in **zero of 1975 logs** and read exactly like a clean sweep:
a JUnit runner exits through `System.exit`, and the summary was sitting in
`vm-cli`'s normal-return arm. Had the census not been the thing being checked,
that run would have been reported here as "armed, nothing found".

With the census verified present, a full 1975-class Spring Boot sweep on the
same host, same 12-way parallelism:

```text
logs 1975   census line present 1972   cells decoded 0   named 0
```

The three logs with no census line are the three HANG classes, killed before
the shutdown trailer -- a blind spot worth stating rather than rounding off.

**The Spring Boot runner now arms it by default** (`-NoCorruptCellCensus` opts
out), and reads the census back into its own `summary.md` and stdout:

```text
## Corrupt Value cell census
- logs scanned: 1975
- logs carrying the census line: 1972
- logs with NO census line: 3 (a class killed before the shutdown trailer
  -- HANG/TIMEOUT -- cannot print it)
- cells decoded: 0
- cells named (door or backstop): 0
```

Reporting "logs carrying the line" separately from "cells decoded" is the whole
point: those are different claims, and the first armed sweep run for this chase
had 0 of 1975 carrying it while looking exactly like a clean run. The harness
caught that same mistake once more on its own first outing -- `0 decoded across
0 of 24 logs` -- because the binary under test predated the instrument.

`decoded > named` is surfaced rather than smoothed over. A cell decoded by a
reader with no door, on a thread that exits before its next safepoint, is
counted and not named; the summary says so instead of implying nothing happened.

**So the cell did not recur, and this page still does not name a producer.**
What changed is that it no longer needs a person to be looking: the next
occurrence reports its own door, receiver and Java stack, or -- if it comes
through a reader with no door -- its frames at the next safepoint. And a run
that decodes a cell nobody names now says `decoded=1 reported=0` instead of
saying nothing.

**It cannot be attributed to the branch that was merged that day**, and it
cannot be called pre-existing either. The control arm has zero hits in this
class, but a single unreproduced event in one arm of a one-shot comparison is
consistent with either. Saying more than that would be reading a coin flip.

## What the next person should do

* **Do not start from this class.** Start from the instrument: the read did not
  come through `NativeContext::get_field`, which is the only door
  `CRATONVM_DBG_CORRUPT_CELL` watches. Widening it to the interpreter's own
  `getfield` and to `get_field_typed` is a small change and would have named the
  producer here.
* **`"t/Proxy"` is the lead.** A slot holding class-name bytes, read as a
  `Value`, is the shape of a name/descriptor buffer reached through an object
  field index — the same species as the `String[]` producer (a class-id test
  with no kind check) but at a different door.
* **A quiet run is not a correct run.** The guard reports only cells whose
  discriminant lands out of range; a buffer whose bytes happen to form a valid
  tag is invisible to it. One hit per 1975 classes is a floor, not a count.

## Reproduce

Not reproduced. The observation is:

```bash
apps/spring-boot-suite-runner/run-spring-boot-suite.ps1 -Category all -Parallel 12
# then grep the per-class .err.log files for "corrupt Value cell"
```

Logs kept at `.suite/results/sbregr2-20260822/fixed/logs/` for
`module_spring-boot-kafka.…KafkaMetricsAutoConfigurationTests.err.log`.
