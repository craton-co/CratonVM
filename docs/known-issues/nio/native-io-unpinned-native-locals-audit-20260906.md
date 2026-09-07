# `native-io` unpinned-native-local audit — the parameter half is closed, the local half is not

| | |
|---|---|
| **Status** | OPEN as a population, with the **parameter shape closed**: all 24 assessed, all 24 real, all 24 fixed. 46 local-shape rows remain. |
| **Scope** | `native-io/src` — the crate the `unpinned-native-locals-audit-48-fixed-20260825` pass never covered. |
| **Tool** | `scripts/unpinned-native-local-audit.py` |
| **Started from** | `internal/fixed-bugs/filechannelimpl-construction-window-was-never-rooted-FIXED-20260906.md`, one instance found by chasing a reclaimed `FileChannelImpl` |

## The number this page opened with was wrong

The first version said **66 parameter-shape candidates**. There are **24**.

Forty-two of that 66 were one rule defect: `statements()` reassembles the
function body starting at its signature line, and `gc_capable` reads the
function's own name in `fn foo(` as a *call to `foo`*. For any function that
allocates — most of them, transitively — statement 0 was therefore a GC-capable
call, and every `ObjectRef` parameter used anywhere in the body qualified.

The negative control is what caught it, not inspection: after the commit that
pinned it, `ts_publish_real_backing_map` still reported `this`.

Two further defects were found in the same pass, and one of them is the
dangerous direction:

| defect | what it did |
|---|---|
| the signature line counted as a call to itself | 42 spurious parameter candidates — a rule that is *loud* is at least visible |
| **`(`/`[` counted inside string literals** | a JVM type descriptor is a bracket bomb: `"([BII)V"` has one `(`, one `[` and one `)`, and the `[` never closes. One such line left the statement depth permanently positive and **nothing after it in that function ever closed a statement again**. `bos_side_write_bulk` stopped after 8 statements of 54 and its second `ObjectRef` parameter simply vanished. `[B`, `()[B` and `[Ljava/lang/String;` are everywhere in a native crate |
| `starts_let` read `buf[0]`, which is `""` when a comment opened the statement | a `let x = match … { … };` preceded by a comment was torn at its first `}` after all — the exact tearing the `starts_let` guard exists to prevent |

That is eight rule defects across the two sessions this tool has existed, every
one of them found by a control rather than by reading the code.

## And the claim about where the 2026-08-25 fixes came from was wrong too

That commit message and the first version of this page said the parameter shape
"produced 40 of the 2026-08-25 pass's 48 fixes". It did not. **That pass's rule 2
was a different rule**: *two or more `ctx.invoke_*` on the same receiver in one
scope with no `read_native_pin` between*. It keys on repeated invokes, and most
of what it caught was `let`-bound out of `args`, not declared in a signature.
The two populations overlap; neither contains the other.

## How this rule was calibrated

83 historical commits that added a pin for a **declared** `x: ObjectRef`
parameter in `native-collections/src/lib.rs`, each run twice — the parent
commit must report the parameter, the commit itself must not.

| | |
|---|---|
| **precision** | 81 / 83 silent after the fix — **2 false positives** |
| **recall** | 58 / 77 reported before the fix — **19 genuine misses** |

(6 of the 25 apparent misses are contamination, not rule failure: the parent
commit already pinned that parameter and the later commit only refined it, so
the rule is right to say nothing.)

**75% recall means the population is a floor, not a census.** The dominant miss
class is a use the linear statement scan structurally cannot see:

* **loop-carried** — `pq_sift_up` calls `pq_compare(ctx, this, …)` inside a
  `while`; the next iteration's use of `this` is an *earlier* statement;
* **same-statement** — `watch_event_kind_bit`'s
  `match ctx.invoke_virtual(kind, "name", …) { … }.or_else(|| ctx.read_string(kind))`
  reads `kind` back in a closure that runs *after* the call returns, inside one
  statement. Real, and invisible to a rule that asks "is the use in a LATER
  statement".

Both of those examples are real defects in this crate. Both were fixed here,
found by reading rather than by the rule.

## What a parameter hit means — which is not what a local hit means

`vm_exec::safe_native_call_impl` **pins every argument** of a native call into
`thread.native_pin_roots`, so an object that arrived through a native entry
point is not reclaimable underneath its holder. The "zeroed in place" half of
this family — the half that produced the `FileChannelImpl` with `fd` 0 — does
not apply to it.

What does apply is **relocation**. The pin keeps the object alive and the
native's copy keeps the old address; `safe_native_call_impl` rebuilds the
argument snapshot only for collections it runs *itself*, at the three hooks
**before** the callback. A native that re-enters Java breaks that premise, and
Generational young relocates by Cheney copy on the moving path and by selective
promotion even on the non-moving one — so `CRATONVM_GC_NO_MOVING_YOUNG=1` does
not rule it out.

Two of the 24 are the *other*, worse class, because their receiver never came
through a native entry at all:

```rust
let list = ctx.alloc_object(cid, 2);        // native_files_read_all_lines: never rooted
al_init(ctx, list);                         // allocates the backing array
for line in &lines {
    let line_str = ctx.create_string(line); // allocates, once per LINE
    al_add(ctx, list, Value::Object(Some(line_str)));
}
```

`al_init` and `al_add` take a `this` nothing has rooted, and `al_add` also holds
`elem` — the String allocated one line earlier — across its own growth
allocation. `Files.readAllLines` on a file with more lines than the default
capacity is the reclamation class, in a loop.

## The 24, and what each one is

All 24 were read. **All 24 are real.** Grouped by what opens the window:

**Caller's object is unrooted (reclamation + relocation)**
`al_init` · `al_add` (`this`, and `elem`, which is a `Value` the rule cannot
match at all)

**Blocking region — a PEER thread's collection runs to completion**
`sc_connect_inner` (`this` across `begin_blocking_region` around a blocking OS
`connect()`, up to the 30 s timeout — the widest window in the crate) ·
`sc_connect_unix` · `sc_connect_bound` (blocking region around a poll loop that
runs to the connect timeout)

**Re-enters Java, then reads the receiver again**
`parse_inet_address` · `read_process_redirect` (and `file`, across `append()`) ·
`decode_socket_address` · `decode_resolved_literal` · `bis_fill` (`this`, the
buffer being filled and the inner stream — the single-byte fallback invokes once
per byte) · `bos_side_write_bulk` (`this` and `src`) · `write_bytes` ·
`buffer_and_maybe_flush`

**Allocates, then stores through the receiver**
`fis_ensure_fd_object` · `fis_backfill_constructor_fields` (and `path_obj`, an
`Option<ObjectRef>`) · `fis_open_path` · `baos_ensure_capacity` ·
`sw_set_count` · `sw_ensure_capacity` · `caw_ensure_capacity` ·
`open_and_register` · `write_handle`

**Hands the stale reference back to Java**
`ssc_bind_unix` — `create_string`, then a store through `this` *and*
`Ok(Some(Value::Object(Some(this))))`. A stale return value lands on the
caller's operand stack, where nothing will ever correct it.

## Awareness is not coverage, twice more

The 2026-08-25 page has a section by that name. Two more instances here, and
both authors were reasoning about exactly this hazard:

* **`fis_open_path`** carries a doc comment saying the `File` overload
  deliberately avoids `create_string` because "that allocation can move `this`
  out from under the Rust local — the receiver is pinned as a native ARG and the
  collector remaps the pin, but not a bare copy of it". It then holds a bare
  copy across `fis_backfill_constructor_fields`, which allocates a
  `FileDescriptor` and possibly a `closeLock`.
* **`sc_connect_inner`**'s phase-trace comment names `create_string` as the
  thing that "ALLOCATES and can therefore reach a collection" — as a debugging
  hint about where a stall lives. The `cf_set` on the very next line stores
  through a pre-allocation copy of `this`.

## What is fixed

All 24, plus the two the rule cannot report (`watch_event_kind_bit`,
`write_handle`), using the tree's own idiom — `pin_native_root` /
`read_native_pin` / `unpin_native_roots`. The audit reports **24 parameter rows
before and 1 after**.

The one that remains is `write_bytes`, and it is a false positive that survives
its own fix: the rule flags the window opened by `ensure_open`, which allocates
only on the path that throws and returns `Err`. The window that is actually real
in that function is `encode_for_stream`, which asks the stream's encoder for its
malformed/unmappable actions through `invoke_virtual` and returns normally. That
one is fixed; the row stays because arguing it away would mean teaching the rule
that a call which allocates only on its error path is safe, and that is a
dismissal a grep should not be allowed to make.

Pins are released on every path rather than left to the native-call floor
wherever the helper can run more than once per native call — `al_add` runs once
per line, `bis_fill`'s fallback once per byte. The 2026-08-25 page records
introducing a pin leak in exactly that shape.

## What is NOT fixed

**46 local-shape rows.** Same crate, same family, unassessed beyond the reading
recorded in this page's first version (roughly 35–40 of the original 48 judged
real). `alloc_process_handle`, `alloc_zip_entry`, `alloc_afc_channel`,
`make_path_stream`, `pipe_open`, `native_ws_new`, `dc_inet_socket_address`,
`ssc_accept_impl`, `sc_socket`, `build_zip_entry_list`,
`native_jarfile_get_manifest`, `native_file_list`, `read_process_environment` …

**The shapes the rule still cannot match.** `--opt` scans
`Option<ObjectRef>` / `&[ObjectRef]` / `Vec<ObjectRef>` / `Value` /
`&[Value]` parameters and finds **2 more** rows (`wrap_afc_future`,
`alloc_process_handle`). That is deliberately not folded into the default count:
they carry a reference the same way but each needs a different fix. It is also
the useful negative result — `args: &[Value]` is not where the remaining mass
is, because natives destructure `args` at the top and then work through derived
LOCALS, which rule 1 already covers.

**And the 19 genuine misses.** Loop-carried and same-statement uses are not
rare; two of the 26 fixes in this pass are of exactly that shape and were found
by reading. A crate swept only by this rule has not been swept.

## Gates

`cargo test -p cratonvm-native-io`: 534 passed / 0 failed (unchanged).
`regression-suite/run.sh` green.
