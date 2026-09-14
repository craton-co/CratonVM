# WORKER-5 NOTE 9 — the untyped-alloc ratchet is RED on the handoff tip again, `089329af7` owns it, and this lane did not re-baseline

**Status: OPEN (drift alarm), MEASURED.** Lane WORKER-5, 2026-08-22. Owning the
gate, not the growth.

This is `WORKER-4-NOTE-1`'s procedure applied by the lane that owns the gate,
which is the case it did not cover: WORKER 4 showed how a lane that TRIPS the
ratchet should attribute it without touching the baseline. The same rule binds
the gate's own author.

---

## 1. The verdict

`scripts/untyped-alloc-ratchet.sh` on `claude/jdk-only-mode-handoff-09b48c`:

```text
  objects: 28 (baseline 29)     IMPROVED: object down 1
  arrays : 130 (baseline 130)
  reach  : 618 (baseline 617)   TRIPPED: reach GREW by 1
  reach MOVED per function (a still TOTAL can hide an offsetting pair):
    alloc_ref_array         157 -> 160
    alloc_singleton           3 -> 0
    chm_init_segments         6 -> 7
```

`rc=1`. The gate is a blocking CI job, so this is worth resolving rather than
carrying.

## 2. It is NOT this lane, and that is measured rather than asserted

Two independent checks, because "my commits look unrelated" is an argument:

1. **A detached worktree at the tip, carrying NONE of this lane's commits** —
   WORKER 4's own experiment, which costs one command:

   ```text
   git worktree add --detach /tmp/x FETCH_HEAD
   cd /tmp/x && scripts/untyped-alloc-ratchet.sh
     objects: 28 (baseline 29) · reach: 618 (baseline 617) · TRIPPED · rc=1
   ```

   **Origin's tip is already red, byte for byte the same numbers.**

2. **This lane's four unlanded commits touch ZERO files in the crates the
   ratchet scans** (`ab038275f`, `d555cc70f`, `4903bcf62`, `d529c1d90` — the jit
   sink counter, the probe gate's `grep -a`, W4Data's NUL bytes, and NOTE-8).
   Counted, not eyeballed.

## 3. Who owns it

`089329af7` — *"the retirement dial never won over the force gate, and a CHM
with no segments called a dropped store a fresh insert"*. `chm_init_segments`
and `alloc_ref_array` are both `native-collections/src/lib.rs`;
`alloc_singleton` is `native-builtins/src/shared_secrets_bridge.rs`.

**Two of the three moves are almost certainly correct**, and the third is a
genuine improvement:

* `alloc_ref_array` +3 callers — a CHM that now initialises real segments has to
  allocate them from somewhere;
* `chm_init_segments` 6 -> 7 — one more site in the same fix;
* `alloc_singleton` 3 -> 0 — **gone**, which is the `objects: 29 -> 28`
  improvement.

None of that is adjudicated here. Naming the functions is the whole job; the
owning lane knows whether each is deliberate.

## 4. What was NOT done, and why

**The baseline was not moved.** It is not this lane's growth, and the rule
`WORKER-4-NOTE-1` wrote down applies symmetrically:

> *"a lane that re-baselines another lane's growth destroys exactly the evidence
> the next reader needs."*

The gate's author re-baselining silently would be worse than anyone else doing
it, because it would look authoritative. The fix is a re-baseline **with the
sentence attached**, and `0fb2e1026` is the worked example.

## 5. The gate's third useful outing, and the pattern worth keeping

This is the third time in two days the per-function reach map has named
something a single total could not:

| occasion | the total | what the map said |
|---|---|---|
| 2026-08-22 (WORKER 2) | `reach` UNCHANGED at 629 | `alloc_ref_array` −1 and a new site +1 — a regression and an improvement cancelling |
| 2026-08-22 (WORKER 4) | direct counts unchanged | `alloc_obj` 18 → 10 — a real win, otherwise invisible |
| here | reach +1, objects −1 | three functions moved, two up and one to zero |

**A single scalar cannot be attributed. A per-item map can**, and every one of
these three would have been either invisible or unattributable without it.

## 6. What this does NOT establish

* **Nothing about whether the three moves are correct.** This record attributes
  and refuses to adjudicate. The `alloc_ref_array` growth in particular may be
  exactly right.
* **`reach` is still ONE level of call graph** (the gate's own L1). A caller of
  a caller is invisible; both columns are floors.
* **It does not say CI is broken.** `rc=1` is a drift alarm and the gate's L4
  says so: *"this ratchets DRIFT. It is not the size of the problem."*

## 7. NOMINATIONS

* **N1 — the owning lane should re-baseline `089329af7`'s growth with an
  account**, naming the three functions of §1. Until then the gate is red for
  everyone.
* **N2 — consider whether `alloc_singleton` reaching 0 means its last site went
  away**, in which case the function may be dead and removable rather than
  merely unused by the ratchet's population.

---

### INDEX ROWS (for H0 to move into `INDEX.md`)

* `WORKER-5-NOTE-9` — the untyped-alloc ratchet is RED on the handoff tip
  (`reach` 617 → 618, `objects` 29 → 28). **Attributed to `089329af7`, not to
  the lane that owns the gate**, by a detached worktree at the tip carrying none
  of this lane's commits plus a count showing those commits touch zero scanned
  files. Baseline deliberately NOT moved, per `WORKER-4-NOTE-1`'s rule, which
  binds the gate's author symmetrically. Third occasion in two days that the
  per-function reach map named something the total could not. MEASURED.
