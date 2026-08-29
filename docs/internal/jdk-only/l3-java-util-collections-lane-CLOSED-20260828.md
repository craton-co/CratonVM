# L3 — `java.util` collections: lane CLOSED, 2026-08-28

**This is the retired lane brief.** It was
`HANDOFF-20260828-L3-util-collections.md`, one of the seven lanes
`HANDOFF-20260828-SCOPE.md` opened; it is here because the lane is finished. The
findings live in `docs/known-issues/jdk-only/` — the L3 sweep record and the
method-reference dispatch-door record — because both still pose questions. This
page keeps only what the brief itself said and what happened to it.

## What the brief asked for

> `java/util/Properties` 92 bridge-with-code rows, `TreeMap` 41, `ArrayDeque` 33,
> `LinkedList` 31, `TreeSet` 31, `Hashtable` 24, `HashMap` 30 (11 done),
> `Collections` 26 (4 done), + the `java/util` tail. ~380 rows, 17% — the
> largest lane.
>
> Registrar: `native-collections/src/lib.rs` — 74 570 lines, shared with L4 and
> L6. Merge `origin/dev` often.

It predicted where the yield would be, and it was right about four of five:

| the brief said | what the measurement said |
| --- | --- |
| start with `Properties`; `properties_sidetable.rs` "exists *because* `Properties` does not behave like a `Map`" — a stated justification of the kind that has drifted three times | **8 defects**, every one on the seam between the side table and the CHM mirror the file added later |
| `Properties`: `getProperty` vs `get`, the defaults chain, `stringPropertyNames` filtering, `load`/`store` escapes | **all correct already.** The defects were `keys()` dropping non-String keys, `propertyNames()` filtering where the JDK CASTS, and the six conditional mutators |
| `TreeMap`/`TreeSet`: null key with and without a comparator; `subMap` bounds and write-through; `firstKey` on empty; `ceiling`/`floor` at both ends | **the ANSWERS were all right and the REFUSALS were all missing** — plus `TreeSet.clone()` was a hard crash and the range views were snapshots |
| `ArrayDeque` refuses null elements; the `pop`/`poll` pair is one edge asked twice | **both already correct.** The defect was `removeIf`/`retainAll`/`removeAll`, which were not registered at all and corrupted the deque |
| `Hashtable` refuses BOTH a null key and a null value, unlike `HashMap` — "the classic shared-surface trap … check whether one body serves both" | **the trap is not there.** 136 rows, 0 differences, first run, no fixes |

## What it did not say, and cost a build cycle each

* **The tail is bigger than the named families.** `Locale` 24,
  `ArrayList$SubList` 23, `Optional` 20, `ArrayList` 19, `Date` 19,
  `LinkedHashMap` 19, `PriorityQueue` 13, `LinkedHashSet` 12, `TimeZone` 10 and
  eleven view carriers outweigh `TreeMap` + `Properties` together. Nine of the
  twelve probes ended up being tail probes.
* **`Properties` is 40 owning rows, not 92.** The brief's number counts every
  registration of the name; 52 lose the slot. Its own §5 says to check
  `owns_slot` before editing — the same filter belongs on the SIZING.
* **`x::m` is not `() -> x.m()`** on this VM. Two rows read as a `Properties`
  defect for a build cycle. Its own record.

## Result

1693 probed rows, twelve probes, 59 defects fixed, five residuals recorded, four
builds. Ten of the twelve probes are 0-diff in both modes; the two that are not
carry only the recorded residuals.

Landed on `dev` from `claude/l3-util-collections-20260828`
(worktree `/data/cvm-l3u-20260828`).

One defect found here belongs to another lane and is recorded for it: `new
ConcurrentHashMap<>(-1)` validated nothing, which is why `new Properties(-1)`
did not throw. It is fixed with the same one-line shared guard every other
hash-ordered constructor now calls.
