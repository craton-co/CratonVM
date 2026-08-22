# WORKER-1-NOTE-2 — `RTreeRangeGc`: dev's loose end closed, the range views confirmed, and a 3-walk repro whose arms do NOT match the vector's

**Status: RESOLVED 2026-08-22 — see the section below the measurements.**
Originally filed OPEN. 2026-08-22, pristine `origin/dev` at `a7c22ddc1`,
built on Azure host 2 (Linux, JDK 25). Companion to
`docs/known-issues/gc/bug-zgc-relocation-unmasks-root-collection-gap-rtreerangegc-20260821.md`,
which localises the gap to "the range-VIEW machinery" and stops there.

---

## 1. The record's own loose end, closed

That record ends: *"pristine `origin/dev` was not built and run (a ~57-minute
LTO build). The claim that dev is already red rests on the table above plus the
two controls, not on a direct observation of dev's own binary."*

Built it. **Dev's own binary is red, 3 of 3**, isolated `--Xmx 64m` repro, no
`PASS` line. Every control in that record also reproduces on it:

```text
default (ZGC)      FAIL      generational   FAIL      --nojit          FAIL
--jdk-only         pass      -XX:+UseG1GC   pass      ZGC_RELOCATE=0   pass
```

## 2. MEASURED — the range views are the site; whole-map iteration is not

The record names the range-view machinery as the remaining suspect after
disproving the backing store, but nothing had separated the eleven views the
vector walks in one process. One view kind, repeated over one map, `--Xmx 64m`,
two runs each — every ordinal below is 2/2:

```text
  m.headMap(k)                fails at walk 18
  m.tailMap(k)                fails at walk 18
  m.headMap(k, false)         fails at walk 18
  m.subMap(k, k)              fails at walk  3
  m itself (no range view)    64 walks, CLEAN
```

**The whole-map view never fails and every range view does.** That is the
record's suspicion turned into a control: the defect is in what materialises a
RANGE, not in the iteration machinery both share.

**`subMap` is ~6× more sensitive than its siblings.** It allocates two bound
keys per call where the others allocate one, so 2× would be expected; 6× is not
explained by allocation rate and is the reason it is the repro to use.

### The first bisect was confounded, and saying so is the point

Truncating the vector's own sequence put the first failure at phase 6,
`headMap(k, false)` — and that is **wrong as a localisation**. `headMap(k,false)`
survives 17 walks on its own. Phase 6 is simply where the second collection
falls in that sequence. The guard says so directly:
`last_publish_at_collection=1 collections_now=2`. A collection counter is the
purest per-process latch there is, and `H0-8`'s rule applies without
modification: **anything latched per process is confounded with position.**

## 3. The failure, in the application's own terms

```text
ClassCastException: class TreePhaseBisect$V cannot be cast to java.util.Map$Entry
        at walkMap(...)                       <- `Map.Entry e = it.next();`

ERROR cratonvm::gc::guard: obj="0x2001015fe18" site="checkcast"
  in_published_snapshot=true published_roots=893
  last_publish_at_collection=1 collections_now=2 last_publish_pc=17
  holder=<not found in frames> frames=2 top_frame=walkMap pc=44
```

The iterator handed back a **value object where an entry belongs** — an address
that was an entry before the relocation and is a `V` after it. The published
root snapshot is **one collection stale**: published at collection 1, still
being marked from at collection 2.

## 4. THE PART THAT DOES NOT FIT — two arms disagree with the vector's

This is the reason this note is `OPEN` and not a diagnosis.

| arm | full `RTreeRangeGc` | `TreeRepeat sub 8` |
|---|---|---|
| default (ZGC) | FAIL | FAIL |
| `--jdk-only` | **pass** | **FAIL @3** |
| `-XX:+UseGenerationalGC` | **FAIL** | **pass** |
| `-XX:+UseG1GC` | pass | pass |
| `--nojit` | FAIL | FAIL @2 |
| `CRATONVM_ZGC_RELOCATE=0` | pass | pass |
| no `--Xmx` | pass | pass |
| HotSpot oracle | pass | pass |

**They disagree on `--jdk-only` and on generational, in opposite directions.**

That matters twice over:

* The record concludes *"Strict mode is CLEAN. `--jdk-only` drops the
  substituted collection natives … so the unrooted reference is held by
  CratonVM's own collection substitution."* **My repro fails under
  `--jdk-only`**, where those natives are not in the picture. If it is the same
  defect, that conclusion is too narrow and the gap is in something both modes
  share. If it is not the same defect, there are two.
* Generational is the arm the record says has *always* been red. My repro passes
  there, 2/2.

**So this note does NOT claim to have diagnosed `RTreeRangeGc`.** It claims a
sharper, cheaper repro of *a* range-view relocation gap, and it claims the two
are not yet shown to be the same thing. Reasoning from "I found a 3-walk repro
of it" would be the error this directory keeps recording — a probe reporting its
own reach as the defect.

## 5. Reproduce

```bash
javac -d probes TreeRepeat.java
cratonvm --java-home "$JDK" --Xmx 64m -cp probes TreeRepeat sub 8
```

`FAILAT walk=3`, 2/2. Three walks instead of a 14,014-check vector, and the
failing walk ordinal is stable, so a fix is judged on whether the ordinal moves
rather than on a pass/fail that a 25% flake can supply for free.

## RESOLVED 2026-08-22 -- SAME defect family, and N1 is answered by an A/B

`TreeRepeat` is red 6/6 on a `dev` binary and green 6/6 on
`fix/gc-known-issues-20260822`, on BOTH the arms this note flagged as
disagreeing. One binary per column, `--Xmx 64m`, `sub 8`:

```text
              dev (cratonvm-safehandle-20260822)   fixed (cratonvm-strdoor-20260822)
default       0 pass / 6 fail  FAILAT walk=3       6 pass / 0 fail
--jdk-only    0 pass / 6 fail  FAILAT walk=3       6 pass / 0 fail
```

and this note's own per-view table reproduces exactly on the dev binary and is
uniformly clean on the fixed one, at 40 walks rather than 8:

```text
view     dev, 40 walks              fixed, 40 walks
head     FAILAT walk=18   x2        clean x2
tail     FAILAT walk=18   x2        clean x2
headF    FAILAT walk=18   x2        clean x2
sub      FAILAT walk=7    x2        clean x2
entry    clean            x2        clean x2      <- the whole-map control
```

**N1 -- same or different: SAME family, three defects.** This note's
"disagreement" was real and its caution was right; what it could not know is
that `RTreeRangeGc` is three defects and `TreeRepeat` reaches a different
subset of them than the vector does. That is exactly why the arms disagreed:

* the vector's compatible-only half is an `entrySet()` view whose KIND was
  guessed from its head element and defaulted to "values", plus a
  `tm_sync_native_state` that relocates its own receiver while 34 callers went
  on using the address they passed in;
* the vector's strict-only half is six `TreeSet` range natives that read their
  backing array and bounds before two allocations and pinned nothing;
* `TreeRepeat` walks `TreeMap` range views only and reaches the second of
  those in BOTH modes, which is why it fails in both.

So the parent record's *"strict mode is CLEAN, therefore it is the collection
substitution"* was indeed too narrow, in precisely the way this note suspected:
strict mode was clean **for the vector**, not for the machinery.

**N2 -- why `subMap` is ~6x more sensitive, not 2x.** Not separately measured,
and now moot as a defect signal, but the candidate is on the page: the two-bound
entry points alone route through `tm_refuse_reversed_bounds`, which dispatches a
FULL user comparator before the view is built at all. That is an extra
allocating interpreted call per `subMap` on top of the second bound key, so the
extra factor is a comparator dispatch and not a `new`.

**N3 -- the vector's `--jdk-only` PASS.** Withdrawn as load-bearing: it was
2/2, `WORKER-1-NOTE-1` measured 9 pass / 3 fail over twelve, and this lane
measured 2 fails in 20 on its own binary. All three are consistent with
`WORKER-5`'s framing that "flaky" is a property of a BINARY AND A HOST. The
vector now passes 25/25 in both modes.

Full record: `rtreerangegc-was-three-collection-native-defects-FIXED-20260822`
(internal). The parent page is retired into it.

**What this note contributed, and it was the load-bearing part:** the
whole-map-versus-range-view control. "Every range view fails and `entry` is
clean over 64 walks" is what turned the parent's suspicion into a fact, and the
`entry` row is the one that stays useful -- it is the negative control that
tells a future regression here apart from a general iteration defect.

## NOMINATIONS

* **N1 — settle same-or-different before fixing either.** Run the full vector
  and `TreeRepeat sub` under `CRATONVM_DBG_COERCION=1` and compare the guard's
  `holder` / `last_publish_pc`. If the two disagree there as they do on the
  arms, they are two defects and the record's `--jdk-only`-clean conclusion
  stands for its own.
* **N2 — why is `subMap` 6× more sensitive, not 2×?** Two bound keys explain 2×.
  The remaining factor is the thing to read: it is the difference between the
  one-bound and two-bound paths, and it is where a snapshot is most likely held
  longest.
* **N3 — the vector's `--jdk-only` PASS is now load-bearing** for the record's
  localisation, and it rests on the full vector alone. It deserves the same
  repeat treatment the flake got (`WORKER-1-NOTE-1`): 2/2 is not many.

## INDEX ROWS

- [WORKER-1-NOTE-2](WORKER-1-NOTE-2-rtreerangegc-range-views-and-a-repro-that-disagrees-20260822.md) —
  `OPEN` · **MEASURED on a pristine `origin/dev` build, which closes the parent
  record's one stated loose end: dev is red 3/3 on its own binary.** Separates
  the eleven views the vector walks in one process: **every range view fails and
  whole-map iteration is clean over 64 walks**, confirming the parent's
  suspicion with a control; `subMap` fails at walk **3** where its siblings last
  to **18**, ~6× more sensitive than its extra bound key explains. A first
  bisect fingered `headMap(k,false)` and was **confounded by position** — the
  guard reports `last_publish_at_collection=1 collections_now=2`, so the latch
  is the collection counter, exactly `H0-8`'s rule. **The finding that blocks a
  fix**: the 3-walk repro's arms disagree with the vector's on TWO of eight —
  it FAILS under `--jdk-only` where the vector passes, and PASSES on
  generational where the vector fails. So either the parent's "strict mode is
  clean, therefore it is the collection substitution" is too narrow, or there
  are two defects. Not claimed as a diagnosis of `RTreeRangeGc`.
