# Phase 2's last live lead closed itself, and the probe sweep found five more that retirement is the wrong tool for

**Status: MEASURED 2026-09-08.** Windows 11, JDK 25 (Temurin `25.0.3+9`, the
same image as the oracle), release binary built from `origin/dev`. No
retirement is proposed and none is taken; this is the lead ledger, re-taken.

**Reads with** `phase-2-adjudicated-the-corpus-cannot-decide-a-retirement-20260830.md`,
whose §7 left exactly one live lead, and whose §5 rule — *"a lead measured
against yesterday's binary is a lead about yesterday's binary"* — is the reason
this page exists.

---

## 1. The last lead is closed, and not by a retirement

The 2026-08-30 adjudication produced five improving families and, on
re-measurement 2026-09-01, four had already resolved: three closed in the tree,
and `AbstractReceiverSweep`'s twelve are the FFM carrier's class name, which
that record and `the-ffm-carrier-is-the-vms-own-allocation-shape-20260829.md`
both settle as not-a-defect.

That left `L6MsgProbe`'s 8 — `ConcurrentHashMap` constructor and
argument-validation messages, which armed went to 0. Re-taken today, **unarmed**:

```text
                     d(hs,base) 08-30   d(hs,base) 09-01   d(hs,base) TODAY
L6MsgProbe                  8                  8                  0
```

`L6MsgProbe` is now byte-identical to HotSpot with nothing armed — all 33 rows,
including every row the lead was about (`new ConcurrentHashMap<>(-1)`,
`(16, 0f)`, `(16, NaN)`, `(16, .75, 0)`, `(null)`, `putAll(null)`,
`containsValue(null)`, `contains(null)`, `values().contains(null)`,
`newKeySet(-1)`, `replaceAll` → null). The armed run is 0 as well, which is now
uninformative: there is no distance left for a retirement to close.

**So the adjudication's lead list is empty.** Its structural findings stand —
the corpus cannot decide a retirement, a per-class sweep cannot predict a set,
a retirement's blast radius is its class's USERS — but the one candidate it
produced has been overtaken by ordinary fixes landing in the tree. Phase 2 needs
new leads, not more analysis of that set.

**A measurement bug of my own, recorded because it nearly became the finding.**
The first comparison reported `d(hs,base) = 66` on a 33-row probe — every line
differing while the text was identical character for character. It was CRLF:
the HotSpot arm's output and the VM's were compared without `tr -d '\r'`, which
`scripts/jdk-only-strict-probes.sh` does and my ad-hoc `diff` did not. A whole
file marked different is the shape to distrust; a real divergence is a few rows.

## 2. Five new leads, from the corpus nothing had ever run

`the-io-probe-families-swept-on-windows-for-the-first-time-20260907.md` and the
sweep behind it ran the 107 `apps/probes/` files no gate schedules through the
three-arm harness. Five came back in a category the Phase 2 sweep could not
produce, because the dial cannot express it:

> **`--jdk-only` is byte-identical to HotSpot, and `--real-jdk` is not.**

Strict mode drops the `SyntheticStub` and runs the real class; compatible mode
runs the native. So on these rows the native **is** the defect, and the
bytecode underneath it is already right — measured, not argued.

| probe | what the native gets wrong (compatible mode) |
|---|---|
| `StreamReuseProbe` | **26 rows.** A stream that has already been consumed silently works again: `IntStream.range(…).count()` twice returns `3` where HotSpot throws `IllegalStateException: stream has already been operated upon or closed`. Also `mapToInt`, `LongStream.range`, singleton/empty sets, `ArrayDeque`, array streams, `parallelStream`. |
| `ScannerShadowSweep` | `Scanner.locale()` returns **null**, so `locale().equals(…)` is an NPE where HotSpot answers `true`; `NoSuchElementException` carries `msg="no more elements"` where HotSpot's is `null`. |
| `ItrCarrierCensus` | `Vector.iterator()` hands back a `java.util.ArrayList$Itr`; `List.of(…)` and `unmodifiableList(…)` hand back `cratonvm.internal.UnmodifiableListItr` instead of `ImmutableCollections$ListItr` / `Collections$UnmodifiableCollection$1`. |
| `L4Diag` | `truncate(-1)` throws `IllegalArgumentException: Negative size: -1`; HotSpot's message is `Negative size`. (Strict is right here because `sun/nio/ch/FileChannelImpl.truncate` is already in `RETIRED_SHADOW_PHASE2_TRIPLES` — see §3, this is the shape.) |
| `L4Reach` | one row: `ok=358 threw=12` against HotSpot's `ok=357 threw=13`. |

`StreamReuseProbe` is the one to read first. Silent stream reuse is not a
message difference — a consumed stream answering a second terminal operation is
a wrong result in the **default** mode, and the exception it swallows is the
JDK's only signal that a program has a real bug.

## 3. Retirement is the wrong tool for all five, and the reason is worth writing down

The tempting move on reading "strict is right, compatible is wrong" is to reach
for `retired_shadow.rs`. **It would change nothing on any of these rows.**

`triple_is_retired_shadow` re-tags a registration as `SyntheticStub`.
`--jdk-only` refuses `SyntheticStub` and the real class runs; **compatible mode
registers and runs it exactly as before.** Retirement is a strict-mode
instrument by construction, so it cannot repair a defect whose whole signature
is *strict already correct, compatible wrong*.

`L4Diag` is the worked example: `sun/nio/ch/FileChannelImpl.truncate` is
already retired — it is the single entry in `RETIRED_SHADOW_PHASE2_TRIPLES` —
and its row is still divergent here, because the retirement moved strict and
left compatible on the old native.

**The fix for these five is to the native itself**: correct it, or delete the
registration so both modes reach the bytecode strict already proves right. That
is a larger and more valuable change than a retirement, and it is per-family
work with its own blast radius, so nothing is attempted here.

## 4. What this does NOT claim

* **No retirement is proposed, taken, or ruled out for any other class.** §3
  says only that retirement cannot address *these five*.
* **The five are leads, not diagnoses.** Each is one probe's rows against one
  oracle on one platform and one JDK. None has been shrunk to a registration
  site, and the row counts are that probe's, not a family census.
* **`L4Reach`'s single row is not characterised at all** — a one-row count
  difference names no method.
* **Phase 2's surface is untouched.** 1477 native-won triples over 270 classes
  as the 2026-08-30 adjudication measured them; this page moves the LEDGER, not
  the surface.
