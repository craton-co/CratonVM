# WORKER-5 NOTE 12 — the stale-receiver audit across all seven native crates, and the one the tree had already half-fixed

**Status: MEASURED. One fix, one gate, sixteen findings handed over.** Lane
WORKER-5, 2026-08-22. Answers `WORKER-5-NOTE-11` N2.

`WORKER-5-NOTE-10` fixed `tm_sync_native_state` and `WORKER-5-NOTE-11` audited
`native-collections` by hand. N2 asked for the other six crates. This is that
sweep, and it is a **committed gate** rather than a one-off list:
`scripts/stale-receiver-audit.py`.

---

## 1. The shape

1. takes a receiver `ObjectRef` **by value**;
2. can **allocate** (so a moving collector may relocate it);
3. returns `()` / `Result<(), _>` — it **cannot hand the refreshed reference
   back**.

(3) is load-bearing. A funnel returning `ObjectRef` is safe by construction: the
caller cannot ignore the new value without ignoring the result. That is why both
fixes so far were signature changes to `&mut ObjectRef` — it makes every
unconverted caller a **compile error** rather than something a grep must find
again.

**A match is not a defect.** The defect needs a call site that REUSES the
receiver afterwards, and that is what the gate counts.

## 2. The sweep

```text
files 253   fns 16,768   allocating 5,147   matching the shape 131
WITH call sites that reuse the receiver:  17 fn(s), 44 site(s)
```

| crate | fns with reusing sites |
|---|---:|
| `native-builtins` | 14 |
| `native-io` | 2 |
| `native-awt` | 1 |
| **`native-collections`** | **0** — clean after §3 |
| `native-api`, `-crypto`, `-security` | 0 |

The largest are `capture_inheritable_tl_at_construction` (10 sites),
`p54_huc_do_request` (6) and `ws_require_open` (4). Two were verified by hand
rather than trusted to the tool:

* **`capture_inheritable_tl_at_construction`** reaches allocation through
  `snapshot_inheritable_tl_entries`, whose own comment says pass 3 calls
  `childValue`, *"which is arbitrary application bytecode"*. Its callers then do
  `ctx.object_num_fields(this)`. Real shape.
* **`p54_huc_do_request`** performs an HTTP exchange; its callers then do
  `ctx.get_field(this, 6)`. Real shape.

**They are NOT fixed here.** `native-builtins`, `native-io` and `native-awt` are
other lanes' surfaces, there is no reproduction for any of them, and a
44-site migration across 700k lines would be unreviewable. They are handed over
with the gate that found them.

## 3. The one that WAS fixed, because the tree had already half-fixed it

`tm_migrate_fast_to_array` (`native-collections`) is the exception, and it is
worth the space:

* it **already knew** its receiver moves — it declares `mut this` and reassigns
  it from `tm_install_backing_array`'s refreshed value;
* taking the receiver **by value threw that refresh away at the return**;
* of its two call sites, **one had been protected by hand** with the reason
  written out —

  > *"The migration allocates (and can move this map); pin and refresh the
  > receiver across it."*

  — and **the other had not**, going straight on to `tm_state(ctx, this)`.

That is the "correct helper exists and one call site uses it" shape this
directory already has a name for. The receiver is now `&mut`, the write-back
covers both exits including the early `return`, and the hand-written pin at the
protected site is retired because the funnel does it. Neither caller can forget
again.

`native-collections` now has **zero** functions of the shape with reusing call
sites.

## 4. The gate

`scripts/stale-receiver-audit.py` — report, `--detail`, `--update`,
`--selftest`, `--depth`. Baseline at
`scripts/baselines/stale-receiver-sites.txt` (17 fns / 44 sites). It watches the
POPULATION; it does not rank it, because ranking needs reproductions and the two
fixed instances disagree (the TreeMap one failed 100%, `resync_view_set` could
not be made to fail at all).

**Its failure path is exercised**, not assumed. A synthetic allocating funnel
plus a caller that reuses the receiver, appended to `native-io/src/lib.rs`:

```text
  WITH call sites that reuse the receiver: 19 fn(s), 46 site(s)
  TRIPPED: w5_audit_canary_funnel now has 1 reusing site(s) (baseline 0)   rc=1
  [restored]                                                              rc=0
```

`--selftest` needs no tree: six cases covering depth-0 detection, a
comment-only mention, the three caller shapes that are NOT stale uses (rebind,
pin re-read, prose), and a funnel that returns the ref.

### 4.1 The gate found a defect in the gate

The first run reported `seed_buffer_byte_order` as a **directly allocating**
candidate. It does not allocate: `ctx.alloc_object` appears in its **doc
comment**. Three of the first 21 candidates were prose. The detector now reads
CODE only, which is the same error this lane made in `check-probes.sh` (a
`grep` without `-a` reading `Binary file … matches` as a type name) and in its
own first NUL-equivalence proof. Stripping comments cut 21 candidates to 18 and
`allocating` from 5,323 to 5,147.

## 5. What this does NOT establish

* **No reproduction for any of the 17 remaining.** They match the shape; none
  is shown to produce a wrong answer. `resync_view_set` (NOTE-11) is the
  standing example of a matching shape that could not be made to fail.
* **Allocation reachability is depth 1 by default.** A funnel that allocates
  three helpers deep is invisible at that setting; `--depth 2` widens it and was
  not used for the baseline, so the baseline is a FLOOR.
* **`ALLOC0` is a name list.** An allocator spelled a way it does not name is
  missed. It counts `ctx.invoke*` as allocating, which is right (interpreted
  Java allocates) but coarse.
* **The reuse window ends at the enclosing function**, found by the next `fn` at
  column 0. A nested `fn` or a macro body would end it early.
* **Only the FIRST post-call use is reported.** A site whose first use is
  harmless but whose second is a deref reads as a hit either way — the count is
  of SITES, not of proven derefs.
* **`native-collections` being at zero is about this shape only.** It says
  nothing about receivers passed to functions that take `&mut` already, or about
  the other GC hazards this directory records.

## 6. NOMINATIONS

* **N1 — the 14 `native-builtins` sites want their owner.** Start with
  `capture_inheritable_tl_at_construction` (10) and `p54_huc_do_request` (6);
  §2 verified both reach allocation. `scripts/stale-receiver-audit.py --detail`
  prints every site with the line that decides it.
* **N2 — 2 in `native-io`** (`ws_require_open`) **and 1 in `native-awt`**
  (`sync_raster_pixels`).
* **N3 — run the gate at `--depth 2` once** and decide whether the wider
  baseline is the one worth freezing. It was left at 1 because depth 2 was not
  measured, not because 1 is known to be right.
* **N4 — wire the gate into CI** beside the untyped-alloc ratchet. It needs no
  build and no JDK, and it runs in about a minute over 253 files.

---

### INDEX ROWS (for H0 to move into `INDEX.md`)

* `WORKER-5-NOTE-12` — the stale-receiver shape swept across **all seven native
  crates**: 16,768 fns, 131 match the shape, **17 have call sites that reuse the
  receiver** (44 sites — 14 `native-builtins`, 2 `native-io`, 1 `native-awt`),
  handed over unfixed with a reproduction-free warning. **One fixed:**
  `tm_migrate_fast_to_array`, which already reassigned its own `this` and threw
  the refresh away at the return, and whose two call sites were split one
  protected / one not. `native-collections` is now at zero. New gate
  `scripts/stale-receiver-audit.py` + baseline, failure path EXERCISED, and its
  own first run reported three candidates that were `ctx.alloc_object` in a DOC
  COMMENT. MEASURED.
