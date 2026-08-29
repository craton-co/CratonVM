# L2 residuals — both priced now, and the one item that is not this lane's

**Status: OPEN, ONE item, and it is not in this lane's families.** N1 and N2
are both answered with numbers and are kept below as the record of what the
numbers were; N3 stands as a guard. The live item is §4.

**Superseded status line:** OPEN, two items, neither a correctness question. 2026-08-28. The
correctness half is closed and recorded in the retired
`l2-strings-eighteen-defects-five-root-causes-and-the-writer-half` write-up: 747
probe rows, 0 differing lines against HotSpot `jdk-25.0.4+7` in both
`--jdk-only` and compatible mode, gates and the three regression arms green.

This page exists because that record ends with things a reader looking for open
work in `docs/known-issues/` would otherwise never find.

---

## N1 — ANSWERED, and it cost two more builds to answer honestly

`probes/SbLayoutBench.java` against the exact control this branch contains:
`f52fa3fa6` has the null-contract fixes and the `StringBuffer` retirement but NOT
the layout migration, so applying only this lane's two string files to that same
worktree isolates the migration and leaves `origin/dev`'s concurrent JIT work out
of the pair entirely. ABBA-interleaved, six rounds, twelve samples per arm.

**The first measurement said the migration was 2.0x to 3.5x SLOWER**, and it was
right:

```text
                A (no migration)      B (first migration)     B/A
appendString     812 ns/op             1613                   2.0x
appendChar       408                   1020                   2.5x
appendInt        438                   1118                   2.6x
charAt          1316                   4050                   3.1x
inflating       1068                   3766                   3.5x
```

Two causes, both in the helpers rather than in the design:

* **every helper re-derived the layout.** `sb_append_units` alone reached
  `sb_layout` four times — directly, through `sb_count_units`, through
  `sb_capacity_units`, and again for the coder — and each of those re-read
  `value`, re-asked its element type, and for a compact receiver resolved
  `coder` BY NAME under the class-manager read lock;
* **the compact write stored one array element at a time**, where the synthetic
  path it replaced used a single bulk `write_char_array_from`.
  `append("abcdefgh")` went from one bulk call to eight VM dispatches, and to
  sixteen once the builder inflated.

`SbView` (one resolve, handed down), `write_byte_array_from` /
`read_byte_array_into` for the compact payload, and reading `coder` at slot 1
with a self-checking fallback to the name lookup, give:

```text
                A (no migration)      B (migration)           B/A
appendString     960 [ 773-1021]       944 [ 793-1017]        0.98   ranges overlap
appendChar       522 [ 468- 592]       558 [ 458- 589]        1.07   ranges overlap
appendInt        582 [ 526- 639]       607 [ 567- 665]        1.04   ranges overlap
toString         170 [ 164- 175]        24 [  22-  30]        0.14   6.9x FASTER
charAt          2394 [2227-2830]      2918 [2689-3300]        1.22   ranges overlap
inflating       1872 [1680-2012]      2018 [1826-2125]        1.08   ranges overlap
```

**Five of the six shapes show no difference this instrument can resolve, and
`toString` is 6.9x faster** — a LATIN1 payload is half the bytes and they come
back in one bulk read instead of a `get_array_element` per character.

**`charAt` is the honest residual.** Its ranges overlap, so the instrument
cannot separate the two, but its MEDIANS are separated by 22% and it is the
noisiest shape in the set (the control's own range spans 27%). The remaining
structural difference is exactly one extra `get_field` per call — the compact
path reads `coder` where the synthetic one had nothing to read. Whether that is
22% or 0% needs an idle host; this one carried a load average between 8 and 16
for the whole measurement, which is stated because it is the reason no stronger
claim is made.

## N2 — ANSWERED: the retirement is correct and costs 2.0x-3.4x, so it stays DECLINED

`java.lang.StringBuffer`'s 62 registrations were retired because every one of its
methods is a `synchronized` delegation to `super` or a body touching only its own
`toStringCache`/`count`. `StringBuilder`'s methods are the same thin delegations,
so the same argument applies to its 62 rows — and it would halve this family's
remaining shadow surface.

It was measured instead of argued. `CRATONVM_ENFORCE_NATIVE_SHADOW` scoped to
the three classes IS the retirement minus the registration edit — checked, not
assumed: armed, the families go from 39 `native-won` / 36 `bytecode-won` to
**0 / 67**, with 38 triples flipping outcome.

**Correctness: free.** `RStringBuilderContent` passes all 118 checks armed
(`WORKER-3-NOTE-3` §3 measured it dying after 56 with an `ArrayStoreException`),
`RStrings` / `RJdkStringCodePoints` / `RJitStringLayout` all pass armed, the
747-row probe is **0-diff ARMED against HotSpot**, and the whole `--jdk-only`
corpus is **111/112** armed.

**Throughput: 2.0x-3.4x.** Same dial, same binary, ABBA over four rounds, on a
host at load average 6:

```text
appendString   1375 -> 2774   2.02x     charAt      5482 -> 18691   3.41x
appendChar      914 -> 2358   2.58x     inflating   2876 ->  5779   2.01x
appendInt       954 -> 2404   2.52x     toString      28 ->    32   ranges overlap
```

Five of six separate cleanly — non-overlapping ranges, eight samples each way.
`toString` is the exception and it explains the rest: it is the one shape where
the native and the bytecode do the same amount of work, so it is the one shape
where the dispatch route does not decide the cost.

**So the registrations stay, for a measured reason rather than a cautious one.**
The second objection stands untouched: `java/lang/StringBuilder` is the class the
interpreter's `JitIntrinsic::StringBuilder*` door keys on
(`native-builtins/src/intrinsics/mod.rs`), so a retirement has two moving parts.
A future lane that wants these rows gone needs a JIT intrinsic for the bytecode
path, not a re-run of this measurement.

## N3 — three methods are correct because the layout is, not because anyone registered them

`chars()`, `codePoints()` and `compareTo` have **no native registration** and
never appear in a `native-shadows-bytecode` row. They are correct today only
because the payload they read directly is now the real compact `byte[]` with a
truthful `coder`. If the migration is ever narrowed or reverted they go back to
being wrong — silently, and only above U+00FF, which is the range no
happy-path probe visits.

`probes/StringBuilderShadowSweep.java`'s `direct` and `compare` sections are the
guard. The `sb compareTo pair` row in particular is the one that catches it: the
`€` versus `₭` and `€` versus `b` rows PASS on the broken build, because
truncating a UTF-16 unit to its low byte preserves the comparison's sign often
enough to look right.

**The general lesson for the campaign, which is why it is here and not only in
the closed record:** a lane's worklist is the set of triples the report names,
and the report can only name a native that EXISTS. The methods with no native at
all are invisible to it, and they are exactly the ones running real bytecode
against a layout the VM may not have. Read the class's public API against the
registrar's list, not only the report. For this family the gap was six methods;
three were broken and three were fine, and no instrument in this campaign would
have told them apart.

---

## 4 — the LIVE item, and it is not this lane's: `RJdkEnumerations`

The scope brief's known-red list reads *"`RJdkEnumerations` — dev's `a0168ed03`,
bisected, recorded"*, and its table records L6 as having FIXED the
`ConcurrentHashMap.elements()` mechanism behind that. Under `--jdk-only` the
vector still fails, and it is neither of those things. MEASURED on the final
binary, running the vector directly:

```text
unarmed  rc=1  NoClassDefFoundError: cratonvm/internal/ArrayListViewItr
armed    rc=1  NoClassDefFoundError: cratonvm/internal/ArrayListViewItr
```

Identical with the builder enforcement dial armed and unarmed, so it is not a
string item at all. It is a **fabricated compatibility class that `--jdk-only`
refuses** — the Phase-1 shape ("a fabricated receiver kills its caller"), not a
Phase-2 shadow-retirement one — and it is the last red on the `--jdk-only` and
`SUITE=all` arms that every lane has been writing off as known.

**Before starting: the obvious mint site is not the one.**
`native-collections/src/lib.rs:7206` (`alloc_arraylist_iterator`) already carries
the refusal landing, added for exactly this symptom and documented there at
length — it falls back to `real_snapshot_iterator` over an exact-length copy. So
the surviving request comes from elsewhere; `grep AL_VIEW_ITR_CLASS` leaves the
three `r.register(AL_VIEW_ITR_CLASS, …)` rows as the candidates, which is a
native registered on a fabricated class NAME, where the name may be resolved for
DISPATCH rather than allocated.

**Unowned.** L3 owned `java.util` and is closed.
