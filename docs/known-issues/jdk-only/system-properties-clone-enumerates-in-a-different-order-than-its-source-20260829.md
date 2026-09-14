# `System.getProperties().clone()` enumerates in a different order than the receiver it was cloned from

**Status: FIXED 2026-08-30 by lane L3.** `apps/probes/PropsOrderSweep` is
0-diff in both modes, 23 of 23. The fix is the FIRST exit below, taken at the
level its own objection lives at: `native_properties_clone` gates its backing
rebuild on the RECEIVER having a CHM, so a clone is as CHM-less as the thing it
was cloned from. The objection -- that the null `map` is deliberate so
un-overridden JDK bodies fail loudly -- argues for the SOURCE keeping a null
map, and the source keeps it; it cannot argue that a clone should differ from
its source on the very property the loudness depends on. The probe also gained
the terminal `DONE` marker it lacked, without which a truncated run read as a
clean diff.

Everything below is the record as written.

**Status when written: MEASURED, OPEN, NOT MINE TO FIX 2026-08-29.** Found while fixing the
`only_order_insensitive_functions_read_the_unordered_snapshot` red on `dev`;
reported here rather than fixed, because the fix is a change to how a CHM-less
`Properties` is cloned and that belongs to whoever owns
`properties_sidetable.rs`.

`apps/probes/PropsOrderSweep.java`, 23 rows, run against HotSpot 25.0.4+7 in
both modes. **22 of 23 match. This is the 23rd:**

```text
17 System clone order equals System order   HotSpot |true|   CratonVM |false|
```

Both `--jdk-only` and compatible mode give `false`.

## It is pre-existing, and that took a control to establish

The row was first seen on a binary carrying my `ordered_snapshot_kv` change, so
it could have been mine. A second binary, built from the same tree with only
that change reverted, answers **identically**:

```text
CONTROL vs HotSpot        2 differing lines   (this row, and only this row)
CONTROL vs the changed binary   0 differing lines
```

So the row predates the change, and the change moves nothing on this probe.

**The first control build did not count and nearly passed as one.** Its `ls`
showed the binary's mtime unchanged from the previous build — the same
`198639888` bytes at the same `15:25:05` — so the "control" was the changed
binary compared against itself, and it printed a clean `0 differing lines` that
meant nothing. `docs/known-issues/` already records this: check the BINARY's
timestamp, not the build's exit code. The real control is `198639864` bytes at
`15:45:17`.

## What it is

Since JDK 9 `Properties` enumerates through a private `ConcurrentHashMap`. This
VM keeps entries in an identity-keyed side-table instead, and the CHM is
populated lazily by the write paths — so:

* `System.getProperties()`, the synthetic singleton, has **no CHM**, and
  enumerates in side-table insertion order. `ordered_snapshot_kv` documents this
  case and deliberately returns that order unchanged.
* its **clone** gets one, because `native_properties_clone` rebuilds the clone's
  backing through `mirror_loaded_entries_to_properties_backend`, which creates
  the CHM when absent.

So the source enumerates one way and the clone another, on a pair HotSpot keeps
identical: there, `clone.map = new ConcurrentHashMap<>(map)` copies a map that
already exists, and both sides are CHM-ordered.

Note the asymmetry is only visible on a receiver whose CHM is absent. A
`Properties` that has ever been written through has one, and its clone matches —
which is every row of this probe except 17, and every row of
`PropertiesShadowSweep`'s 184.

## Why it is left open

The two exits are not obviously equivalent and both are behavioural:

* **give the clone no CHM either**, matching the receiver — but a real
  `Properties.map` is what un-overridden JDK bodies dereference, and the null
  `map` is deliberate (`register_properties_sidetable`) precisely so those fail
  LOUDLY rather than silently reading an empty map;
* **give the singleton a CHM**, so both sides are ordered by it — which changes
  what `System.getProperties()` is, across the whole VM.

Neither is a gate-quieting edit, and the lane that owns this file has the
context to choose. `PropsOrderSweep` is in the tree so whoever takes it has the
assertion already.

## Reproduce

```bash
J=/data/jdkimages/jdk25-linux/jdk-25.0.4+7
javac -d /tmp/pos apps/probes/PropsOrderSweep.java
(cd /tmp/pos && $J/bin/java -cp . PropsOrderSweep)                > hs.txt 2>/dev/null
(cd /tmp/pos && cratonvm --java-home $J -cp . PropsOrderSweep)    > cv.txt 2>/dev/null
diff hs.txt cv.txt
```

Diff on **stdout only**: this VM writes tracing warnings to stderr, and merging
them in turns a 1-row difference into a 31-row one that hides it.
