# RETIRED — a refresh contract in a PARAMETER MODE is laundered by a by-value wrapper

| | |
|---|---|
| **Status** | **RETIRED 2026-09-07.** `--launder` reports **0 helpers / 0 sites** in all four native crates. |
| **Was** | OPEN — 18 laundering helpers, 36 call sites that then reuse their own copy, none fixed. |
| **Tool** | `scripts/unpinned-native-local-audit.py --launder` |
| **Calibration** | still reports the 2026-09-06 `collect_own_property_names` defect AND its caller at `80db5d314^`; silent at `80db5d314` |
| **Dynamic proof** | the sibling page's, shared: `probes/NativeLoopReceiverSweep.java` under `CRATONVM_DBG_GC_STRESS=65536` separates `origin/dev` from this branch 0/5 vs 5/5. See below. |
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

## THE DYNAMIC PROOF THIS FAMILY DID NOT HAVE

Every page in this family carried the same caveat — *"no dynamic proof for any
of them; the difference is that the shape has a proven instance"* — and the
2026-09-06 fixes' own A/B was FLAT on the workload that found them. That caveat
is now retired too, for one of the 25 sites, and it is the site the earlier
sweep explicitly DECLINED.

`probes/NativeLoopReceiverSweep.java` drives the library code that reaches these
natives — `Properties.load/store`, stream terminals, `reversed()`,
`ConcurrentSkipListMap`/`CopyOnWriteArrayList`/`ArrayDeque` growth,
`ListIterator.remove`, `PosixFilePermissions`, `Exchanger`,
`ExecutorCompletionService`, `Executable.getAnnotatedParameterTypes` — with
allocation churn between turns so a collection can land INSIDE a loop rather
than between two of them. Every line it prints is chosen by the program, so
HotSpot 25 is a usable oracle; the whole sweep is byte-identical between
HotSpot and CratonVM.

Two release binaries, `origin/dev` (`ff636f3c1`) and this branch, same machine,
same probe:

| configuration (Generational, `-Xmx256m`) | `origin/dev` | this branch |
|---|---:|---:|
| default | 5/5 pass | 5/5 pass |
| `CRATONVM_DBG_GC_STRESS=65536` | **0/5** — `annotatedParameterTypes.total = 17`, expected 20 | **5/5** |
| `CRATONVM_DBG_GC_STRESS=262144` | **0/5** — same wrong answer | **5/5** |
| `CRATONVM_DBG_GC_STRESS=1048576` | 5/5 | 5/5 |
| `GC_STRESS=65536` + `DBG_FORCE_MOVING` | **0/5** — same wrong answer | **5/5** |
| the above + `DBG_STALE_OBJREF` (quarantine) | **0/5** — **SIGSEGV**, every run | **5/5** |
| G1 instead of Generational, any of the above | 5/5 | 5/5 |

The failing site is `native_executable_get_annotated_parameter_types`, and the
symptom is the one this family is named for: not a crash but a **silently wrong
answer** — three of twenty annotated parameter types lost, because
`make_annotated_type_with_anns` allocates once per turn and the mirrors it is
handed live in a `Vec<ObjectRef>` that no collection rewrites. Turn the
quarantine on, so a stale read faults instead of reading a forwarded header,
and the same defect is a SIGSEGV.

It is Generational-only and it needs the collection to land inside the loop:
a stress interval of 1 MB never reproduces, 256 KB always does. That is
precisely why the four 2026-09-06 fixes' A/B was flat — the window is a few
hundred bytes of allocation wide, and nothing in an ordinary workload aims at
it.

**The row the previous pass dismissed is the row that fails.** Its survivor
table read *"the binding is inside the loop, or is not an `ObjectRef`
(`generic_type_mirrors` is a `Vec`)"*. True of the `Vec` and false of the
defect: `pin_native_root` does not take a `Vec`, it takes an ELEMENT, and the
elements are what go stale.

## What this does NOT establish

**No dynamic proof for any of the 36 LAUNDERING sites.** The proof above is a
loop-carried receiver, not a laundered contract; it is reproduced on this page
because it is the same family, the same probe and the same pair of binaries,
and because it retires the "no dynamic proof anywhere in this family" caveat
both pages opened with. The laundering half still rests on
`collect_own_property_names`, whose crash gdb traced on 2026-09-06 rather than
one reproduced here -- and that defect is in the same file as four of the
loop-rule ones. Fixing a laundered refresh contract is right whether or not a
workload currently reaches it.

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
* `regression-suite/run.sh` -- **92 of 92 passed, 0 failed.**
* `probes/NativeLoopReceiverSweep.java` -- byte-identical to HotSpot 25 under
  both collectors, and 5/5 under every stress configuration in the table above.
