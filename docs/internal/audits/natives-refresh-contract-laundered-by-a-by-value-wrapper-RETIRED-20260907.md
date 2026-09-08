# RETIRED — a refresh contract in a PARAMETER MODE is laundered by a by-value wrapper

| | |
|---|---|
| **Status** | **RETIRED 2026-09-07.** `--launder` reports **0 helpers / 0 sites** in all four native crates. |
| **Was** | OPEN — 18 laundering helpers, 36 call sites that then reuse their own copy, none fixed. |
| **Tool** | `scripts/unpinned-native-local-audit.py --launder` |
| **Calibration** | still reports the 2026-09-06 `collect_own_property_names` defect AND its caller at `80db5d314^`; silent at `80db5d314` |
| **Predecessor** | `natives-loop-carried-stale-receivers-RETIRED-20260907.md` (the sibling page, retired the same day) |

## The shape, restated once

`ordered_snapshot_kv(obj: &mut ObjectRef)` states its contract in its parameter
mode:

> "Taking `obj` by `&mut` is the point: it forces every caller's own receiver to
> be refreshed across the walk instead of silently carrying a pre-GC address
> into the pins and virtual dispatches that follow."

**A parameter mode binds the immediate call only.** A wrapper that takes the
same receiver BY VALUE satisfies that `&mut` with a copy:

```rust
fn helper(ctx: &mut dyn NativeContext, this: ObjectRef) {
    let mut this = this;                 // a copy
    ordered_snapshot_kv(ctx, &mut this); // refreshes the COPY
}                                        // the refresh dies here
```

and the caller walks on with the pre-GC address. This is the shape that
produced an observed crash, not a static finding: `collect_own_property_names`
was exactly this, and the `defaults` read its caller then performed was the
SIGSEGV gdb traced on 2026-09-06.

## What was done

**Every laundering helper's parameter is now `&mut ObjectRef`, and the compiler
found the call sites.** Not a pin-and-re-read shadow: the point of the page is
that the CONTRACT was in the wrong place, so the fix is the signature.

### The flagship

`stream_elements` was `mut stream: ObjectRef` — by value, mutable binding —
while its own back-compat alias's doc comment asserted *"`stream_elements` is
now `&mut`"* and a comment in `native_stream_concat` said the same
("`stream_elements` is now `&mut`, so avoid the closure form"). Two independent
places in the tree believed a contract the signature did not provide, across 85
call sites that pass a receiver and use it again.

It is `stream: &mut ObjectRef` now and the write-back is unconditional — every
arm, including the two that run real bytecode (`stream_apply_chain_full` and
the `toArray` materialisation), ends at
`*stream = ctx.read_native_pin(stream_pin, *stream)` before returning.

One thing fell out of reading it closely: the `materialize_lazy_stream(..)?`
inside used to `?` straight out of the function, past both that write-back and
the `ctx.unpin_native_roots(stream_pin)` that the comment two lines above calls
"single-exit so the `stream` pin is always released". It is a `match` now.

`stream_elements_mut` is deleted. It existed only to spell the contract its
delegate did not provide.

### The rest, to a fixpoint

| crate | helpers converted |
|---|---|
| `native-collections` | `stream_elements`, `prim_stream_values`, `int_stream_elements`, `stream_match`, `stream_elements_concat_bounded`, `ad_grow`, `lbq_ensure_capacity`, `pq_ensure_capacity`, `lli_resnapshot`, `cowal_ensure_lock_and_array`, `cslm_ensure_capacity`, `alloc_view_backing`, `native_map_put_evict_pinned`, `make_view_set_of`, `make_static_key_set` |
| `native-builtins` | `collect_store_entries`, `https_has_session`, `https_peer_chain_or_throw`, `capture_huc_ssl_context`, `capture_huc_ssl_context_for_connection`, `lucene_buffered_checksum_write` |

The last two rows of each list are the interesting ones: **the rule could not
see them until their callees were fixed.** `make_view_set_of` and
`make_static_key_set` only became launderers once `alloc_view_backing` took its
source by `&mut`; `capture_huc_ssl_context_for_connection` only once
`capture_huc_ssl_context` did. The population is a wave, and it was run to a
fixpoint rather than to the original list.

`cslm_ensure_capacity` had a shape of its own. It took `this`, `keys` and
`values` by value and returned `(new_keys, new_values)`. The ARRAYS were fine —
the caller rebound them — and `this` was not, so `native_cslm_put` pinned and
re-read it by hand around the call. All three are `&mut` now, it returns `()`,
and the hand-rolled pin is gone.

## Four rule corrections, and 36 sites -> 10 -> 0

Every one was found by reading rows, and every one was the rule reporting
correct code as a defect.

**1. A destructuring `let` is a rebinding.** `LET` matched `let x =` only.
`rooted_across1` hands its refreshed receiver back as `(this, out)`, and all 21
of its callers spell that `let (this, buf) = rooted_across1(..)`. Not one read
as a rebinding, so all 21 were reported: **21 of the 34 `native-collections`
sites, all false.** `let_binds` now sees tuple, slice and struct patterns.

**2. The rebinding can be on the CALL STATEMENT ITSELF.** The caller-side scan
started at `k+1`, and `let (this, buf) = rooted_across1(ctx, this, ..)` rebinds
the very name it passes, on statement `k`.

**3. A helper that RETURNS the refreshed copy is not laundering** — reported on
its own line rather than dropped, because "returns it" is only sound where the
caller BINDS it, and `let (_, buf) = ..` is back in the laundering shape.

Getting this arm right took two attempts, and the first attempt is the
instructive one. Accepting any `return` that names the parameter excused
`cslm_ensure_capacity`, whose early `return (keys, values);` on the no-growth
path says nothing about the growth path that refreshes them and returns
something else. Requiring only the TAIL expression, and only for a function
with a declared return type, also removes `ad_grow`, `lbq_ensure_capacity`,
`pq_ensure_capacity` and `lli_resnapshot` from the excused set: all four end on
a trailing `if .. { .. }` block that MENTIONS `this` and all four return `()`.

**4. `**` in the glob matched nothing below one level.** `glob.glob(a.glob)`
without `recursive=True` treats `**` as a single `*`, so every invocation in
these pages silently dropped 41 of `native-builtins`' 178 sources — including
`phases_late/`, where a sixth laundering helper
(`lucene_buffered_checksum_write`) was hiding. The tool takes `nargs="+"` and
`recursive=True` now.

## What this does NOT establish

Unchanged from the original page: **no dynamic proof for any of the 36 sites.**
The difference remains that this shape has a proven instance, and it is in the
same file as four of the loop-rule defects. Fixing a laundered refresh contract
is right whether or not a workload currently reaches it.

The one thing the fix DOES establish that a pin-and-re-read could not: the
contract is now in the type. A future helper that forgets the refresh is a
compile error at every call site, not an audit finding.

## Gates

* `--launder` over `native-{builtins,collections,io,api}`: **0 helpers, 0
  sites**; two helpers reported on the informational "hands the refresh back"
  line (`rooted_across1`, `cslm_arrays`), read by hand and correct.
* Calibration at `80db5d314^`/`80db5d314`: unchanged, both directions.
* `cargo test -p cratonvm-native-builtins -p cratonvm-native-collections -p cratonvm-native-io`.
* `cargo clippy --all-targets` clean on all three; `cargo check --features
  synthetic-jdk` clean (two of the touched functions are `cfg`-gated behind it
  and are not type-checked at all without it — the same trap that had `dev`
  unbuildable on Linux on 2026-09-06).
* `regression-suite/run.sh`.
