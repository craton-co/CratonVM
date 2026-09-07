# `native-io` unpinned-native-local audit — both shapes assessed and fixed; the rule's own reach is now the open question

| | |
|---|---|
| **Status** | Both tranches closed. 24 parameter rows + 46 local rows read; **58 real bindings fixed**, 12 false positives classified. The audit reports **7** rows, all of them understood. |
| **Scope** | `native-io/src` — the crate the `unpinned-native-locals-audit-48-fixed-20260825` pass never covered. |
| **Tool** | `scripts/unpinned-native-local-audit.py` |
| **Open** | `--any-binding` reports **139** rows against the default's 7. That gap is this page's remaining finding. |

## The two numbers this page opened with were both wrong

**"66 parameter candidates"** — there were 24. `statements()` reassembles a body
starting at its signature line, and `gc_capable` reads the function's own name
in `fn foo(` as a *call to `foo`*; for any transitively-allocating function that
made statement 0 a GC-capable call, so every `ObjectRef` parameter used anywhere
in the body qualified. 42 of the 66.

**"roughly 35–40 of the 48 locals are real"** — of the 46 that survived the
earlier fixes, **34 are real and 12 are false positives**, which is the same
ballpark by luck rather than by measurement: the reading had not been done.

## What was fixed

**58 bindings across 25 functions.** The parameter tranche is described below
its own heading; the local tranche is one idiom in a dozen dresses:

```rust
let obj = <allocate>;        // nothing in Java refers to it yet
…
let x = <allocate again>;    // ← moves `obj`; under the Generational
ctx.set_field(obj, S, x);    //   non-moving young sweep, zeroes it
Ok(Some(Value::Object(Some(obj))))   // …and this hands the stale one back
```

Ranked by what it costs:

**Returns a stale reference to Java.** `dbb_allocate_direct0`
(`ByteBuffer.allocateDirect` — two `ensure_class_initialized` calls and two
`alloc_object`s between the buffer's allocation and both its
`discover_reference` registration and its return), `sc_socket`,
`ssc_accept_impl`, `ssc_accept_unix`, `native_afc_try_lock`,
`build_byte_array_input_stream`, `native_jarfile_get_manifest`,
`native_ws_new`, `make_path_stream`, `dc_set_option`, `t16_afc_open_legacy`.

**Throws a stale reference.** `throw_invalid_path_exception` allocates the
`InvalidPathException`, then two Strings, then runs its `<init>`, and returns
`MethodCallFailed::ExceptionThrown(exc)` naming the pre-allocation address. A
garbage throwable discovered on the unwind path is the worst place to find one.

**Loops.** `native_file_list` (one `create_string` per directory entry),
`native_files_read_all_lines`, `build_zip_entry_list` (Jasper's TLD scan runs it
121 times per embedded-container start), `native_process_descendants`,
`ws_poll_event_names0_native`, `dc_interface_ipv4`, `read_process_environment`.

**Worst single function.** `dc_inet_socket_address` — six allocations, four
references, every store to an object minted before the allocation that follows
it, and `inet` stored into `socket_holder` five allocations after it was
created.

**A caller the earlier fix did not cover.** `al_init`/`al_add` now pin what they
are handed — but `native_files_read_all_lines` passes an address that is already
stale from the previous iteration's `create_string`. A callee can only pin the
address it is given; the caller has to hand it a live one.

## Awareness is not coverage — a fourth instance, and this one made it worse

`alloc_zip_entry` opens with:

> Materialize every allocation-prone value before allocating the ZipEntry. This
> keeps the fresh entry out of a bare native local across a GC point.

It does. It also leaves `mtime`, `atime`, `ctime` and `name_str` in bare locals
across a `create_string`, a `new_array`, an `ensure_class_initialized` and two
`alloc_object`s — every one of them stored into the entry at the end. The
mitigation moved the hazard from one reference to four. (Three of those four
were invisible to the rule: `Option<ObjectRef>` locals whose use site names the
unwrapped binding.)

## The twelve false positives, by cause

| cause | rows |
|---|---|
| an `invoke_*` RHS binding a SCALAR — `let limit = match ctx.invoke_virtual(target, "limit", "()I", …)` is an `i32`; `let flushed = ctx.invoke_virtual(..).map(\|_\| ())` is a `Result` | 7 |
| a file gated by an INNER `#![cfg(test)]` — `test_support.rs` is a mock context whose whole point is that it has no GC | 1 |
| `err.detail` read as a use of a local named `detail` | 1 |
| branch-exclusive: the only GC is on a path that returns | 2 |
| a torn `let x = match { arm => { let …; … } }`, where the allocation that BINDS `x` reads as one that follows it | 1 |

Seven of those are now filtered — the scalar test keys on the destructuring ARM,
not on any occurrence of a `Value` variant, because the call's own arguments
routinely carry `Value::Object(Some(buf))`. The tear is deliberately NOT fixed:
un-tearing `let … match` would hide `write_handle`, where the allocation and the
stale store share one statement. Noise is the cheaper error.

## The rule's reach is now the finding

Rule 1 requires the BINDING's own statement to be GC-capable. That is not the
defect's definition — it is a proxy for "this local names a fresh object" — and
the control shows what it costs. At the commit before the FileChannelImpl fix,
`native_fcimpl_open` had seven references that fix had to root. The rule reports
**six**: `fd_obj`, bound by `match args.first()`, is invisible because
`args.first()` does not allocate.

`--any-binding` drops the requirement and keeps the type filter. On the control
it reports **7 of 7**, with the post-fix file still silent. On `native-io`:

| | rows |
|---|---|
| default | **7** |
| `--any-binding` | **139** |

So the default rule sees a twentieth of the shape-eligible population, and the
132 it does not see are references that arrived from `args`, from `get_field`,
or from a helper that does not itself allocate — `fd_obj`'s exact shape, which
is the one instance of this family anybody has actually observed failing.

**That is the open item. It is not "132 bugs"** — the type filter is weaker on
that population and the false-positive rate will be higher than the 12-in-46 of
this pass. It is the number that has to be read down rather than assumed away.

## Calibration, unchanged by the precision pass

83 historical commits that added a pin for a declared `x: ObjectRef` parameter
in `native-collections/src/lib.rs`, run at each commit and its parent:
**precision 81/83, recall 58/77** (6 of the 25 apparent misses are contamination
— the parent already pinned that parameter). Re-run after the precision pass:
identical, so the filtering cost no recall.

Rule 1's own control pair — `native_fcimpl_open` before and after `3950eed48` —
holds at 6/7 by default and 7/7 with `--any-binding`, 0 after.

## What the rule still cannot see, restated

* **loop-carried** uses (`pq_sift_up` reads `this` in the NEXT iteration);
* **same-statement** uses (`watch_event_kind_bit`'s `.or_else(|| …)` closure
  runs after the call it is chained onto; `write_handle`'s stale store shares a
  statement with its allocation) — both were real, both fixed, neither reported;
* `Option<ObjectRef>` locals (`alloc_zip_entry`'s three times);
* a `Value` parameter that happens to be an object (`al_add`'s `elem`);
* bindings whose RHS does not allocate — the 132 above.

## Gates

`cargo test -p cratonvm-native-io`: 534 passed / 0 failed (unchanged).
`regression-suite/run.sh` green.
