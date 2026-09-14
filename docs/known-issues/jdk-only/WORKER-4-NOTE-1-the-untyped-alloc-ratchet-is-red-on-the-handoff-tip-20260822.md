# WORKER-4-NOTE-1 — the untyped-alloc ratchet is RED on the handoff tip, and it is not the lane that finds it

**MEASURED 2026-08-22, Linux (Azure host 2).** Filed by WORKER 4 because this
lane hit it while landing, established whose it was, and should not be the only
party that knows.

## The reading

`scripts/untyped-alloc-ratchet.sh` on `claude/jdk-only-mode-handoff-09b48c` at
`381a14036`:

```text
UNTYPED-ALLOC RATCHET (v7)
  objects: 29 (baseline 29)
  arrays : 130 (baseline 130)
  reach  : 618 (baseline 616)
  TRIPPED: reach GREW by 2.
  reach MOVED per function:
    alloc_ref_array                               156 -> 158
```

`alloc_ref_array` is `native-collections/src/lib.rs:3154`.

## Why this is not WORKER 4's, established rather than asserted

`[dev-gate-red-check-pristine-before-blaming-your-merge]`, and the control was
run both ways round:

1. On this lane's branch **after** merging the handoff tip at `11634cd44` — with
   every WORKER 4 commit present — the ratchet read `reach: 616`, **`ok — no
   growth`**.
2. Merging the tip again, at `381a14036`, moved it to 618.
3. The tip at `381a14036` was then checked out into a **detached worktree with
   no WORKER 4 commits in it at all**, and reads `618` there too.

So the growth arrives with the commits between `11634cd44` and `381a14036` —
WORKER 2's `native-collections` round (`TreeMap.root` as a real red-black tree,
`TreeSet.m` as a real backing map, the `Hashtable` view carriers) — in the file
that declares the function the ratchet names. Step 3 is the one that settles it;
steps 1 and 2 alone would only show WHEN it appeared.

## What was NOT done, and why

**The baseline was not re-based.** `scripts/baselines/untyped-alloc-sites.txt`
still says 616. Moving it is the owning lane's call: the ratchet's whole
function is to make growth ARGUE for itself, and a lane that re-baselines
another lane's growth destroys exactly the evidence the next reader needs. The
two new callers may well be correct — a real `TreeMap$Entry` tree has to be
allocated by something — in which case the fix is a re-baseline **with that
sentence attached**, which `08d8b15b8` is the worked example of.

**Nothing was blocked on it.** The gate exits `0` and prints `TRIPPED`; it is a
drift alarm, not a build failure, and its own `L4` says so: *"this ratchets
DRIFT. It is not the size of the problem."* WORKER 4's rounds landed with all
three arms green (`107/107 · 107/107 · 67/67`) and five probes clean.

## The transferable half

**A ratchet that trips on a merge names a FUNCTION, not a lane.** Reading
`alloc_ref_array 156 -> 158` and going looking in one's own diff is the wrong
first move; the cheap first move is a detached worktree at the tip with none of
your own commits in it, which costs one command and answers the question
outright.
