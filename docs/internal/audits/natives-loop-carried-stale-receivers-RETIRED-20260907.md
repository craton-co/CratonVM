# RETIRED — the audit's straight-line rule cannot see a loop, and that is where the one proven defect family lives

| | |
|---|---|
| **Status** | **RETIRED 2026-09-07.** `--loops` reports **0 rows** in all four native crates. |
| **Was** | OPEN — rule added and calibrated, 123 rows unswept; a later pass took 52 and left 29 "survivors" named but unread. |
| **Tool** | `scripts/unpinned-native-local-audit.py --loops` |
| **Calibration** | still reports all four of the 2026-09-06 `properties_sidetable.rs` defects before their fix, and none of them after |
| **Dynamic proof** | **NEW.** `probes/NativeLoopReceiverSweep.java` under `CRATONVM_DBG_GC_STRESS=65536`: `origin/dev` answers `getAnnotatedParameterTypes` **wrong**, 0/5; this branch 5/5. See below. |
| **Sibling** | `natives-refresh-contract-laundered-by-a-by-value-wrapper-RETIRED-20260907.md`, retired the same day |

## The hole, restated once

`scan()` asks "is there a GC-capable statement BETWEEN the binding and the
use", in statement order. Inside a loop body that order is a lie: the last
statement precedes the first on the next iteration, so a GC anywhere in the
body stales every reference the body carries in from outside — including one
used EARLIER in the text, and including one used in the very statement that
allocates.

```rust
for (k, v) in &snapshot {
    put_kv_units(ctx, this, k, v);   // `put_kv_units` calls ctx.force_gc()
}                                    // `this` is stale from iteration 2 on
```

**The only instances of this family anyone has observed FAILING are loop
instances** — the four `properties_sidetable.rs` defects of 2026-09-06, found
by reading a crash backtrace rather than by a rule.

## What was standing when this page was picked up, and what was actually true

The page reported **27 surviving `native-builtins` rows** and a table of nine
named causes. Both numbers were wrong, in opposite directions, and one tool
defect explains most of it.

### `**` matched nothing below one level

`glob.glob(a.glob)` without `recursive=True` treats `**` as a single `*`. Every
invocation written on these pages therefore scanned only `native-builtins/src/*/*.rs`
or only `native-builtins/src/*.rs`, never both — **41 of the crate's 178
sources were never looked at.** With `recursive=True` (and `nargs="+"`), the
true count was 47, not 27.

### Three regexes contained a literal BACKSPACE where `\b` was meant

A `sed` that wrote `\b` into a REPLACEMENT — where GNU sed expands it — put
`0x08` into `scripts/unpinned-native-local-audit.py` before this branch, and it
had been carried since. `0x08` is an ordinary character to `re`, so each
alternative demanded a literal backspace in the Rust source and matched
NOTHING. Three guards were dead, silently:

* **`REREAD`'s `scope.get` arm.** This file's own comment says a rule that
  cannot see the safe spelling reports every correctly-converted site as a
  defect. It could not see it.
* **`leaves()`'s `return|break|continue` test** — so a statement that leaves the
  block was never recognised as leaving, in BOTH rules.
* **`scan_loops`' "a handle is not an `ObjectRef`" skip** — which is why a
  `let h = ctx.pin_native_root(x)` binding stayed a candidate.

Repairing them drops the `properties_sidetable.rs` calibration from 13 rows to
4 before the fix and from 9 to 2 after, with the four hand-found defects still
reported and still gone respectively. Same calibration, a quarter of the noise.

A startup assertion (`assert_no_control_bytes`) now fails the script loudly if
a future in-place edit reintroduces one.

## The 48 rows, read

47 in `native-builtins`, 2 in `native-collections`, 1 in `native-io`, 0 in
`native-api`. Every one was read. **24 were the rule being wrong and 25 were
real**, and the split is the useful output of the exercise: each false-positive
class became a rule correction with a named instance, so the next reader starts
from a rule that has already learned it.

### Five rule corrections, 48 rows -> 25

| correction | the instance that forced it | rows |
|---|---|---:|
| `rooted_across`'s root list IS a refresh — it writes the forwarded address back through the `&mut` | `native_lhm_entry_set` carries `this`/`set` through three of them | 2 |
| `$read(..)` in a `macro_rules!` body is a METAVARIABLE, not a call — `$` joins `CALLEE`'s lookbehind | `register_s2_bytebuffer`'s three loops, where `$read` resolved to some `fn read(` elsewhere in the tree | 3 |
| a binding whose RHS is plainly not a reference (`.as_int()`, `.is_ok()`, `read_string`, a string LITERAL) | `server_id` (an `i32`), `copied` (a `bool`), `retain_class_ref` (a `bool`, twice), `name` (a `String`), `path` (`"java/nio/file/Path"`, twice) | 7 |
| a plain ASSIGNMENT is a rebinding, when nothing GC-capable names the reference after it | `point = next`, `state = ctx.invoke_virtual(..)`, `this = cur` at the foot of `ecs_take`'s body | 5 |
| the pin handle handed to the CALLEE beside the name | `put_str(ctx, map_pin, map, k, v)` re-reads `map` through `map_pin` as its fourth line | 4 |
| already covered by an earlier correction in the same pass | | 3 |

The assignment rule is the one with a real edge, and it is stated in the code:
an assignment only counts when no GC-capable statement NAMES the reference
after it. An assignment early in the body with a collection after it leaves the
next turn just as stale. The assigning statement itself counts as "after" — its
call returns before the assignment lands, which is what makes
`state = ctx.invoke_virtual(.., &[state, *elem])` safe.

### The 25 real ones, fixed

The insertion is the one the earlier sweep used: `let X_pin =
ctx.pin_native_root(X)` before the loop and `let X = ctx.read_native_pin(X_pin,
X)` as the first statement of the body, so the re-read SHADOWS inside and the
outer binding is untouched.

| where | what was carried into the loop |
|---|---|
| `bc_digest_update_counter_virtual` | `digest`, across eight `Digest.update` dispatches |
| `bc_digest_random_next_virtual` | `this`, `digest`, `state_arr`, `seed_arr`, `bytes` — the refill runs bytecode mid-loop |
| `ucl_try_define_local_class` | `loader`, across a condvar `wait_timeout` — the widest window there is |
| `native_p64_ll_reversed` | `this` and every element gathered by `get(I)` |
| `p64_seq_map_edge_entry` | `it`, and `found` — which holds turn k's entry across every dispatch in turns k+1.. |
| `native_p64_lhm_reversed` | the map being filled and every key/value from the old chain |
| `native_executable_get_annotated_parameter_types` | the output array and every mirror in `generic_type_mirrors` |
| `ucp_init_2` | both collections, the source array, `this`, and `elem` between the two `add`s |
| `pd_gather_fold` / `pd_gather_scan` | the folder/scanner and every element (`state` was already safe) |
| `pd_gather_custom` | `state` — NOT reassigned here — plus `downstream`, the integrator and every element |
| `apply_jul_config_entries` | `handler`, an element of a Rust `Vec`, across `new_object_initialized` |
| `randomized_context_for_thread` | `contexts` — **pinned every turn and never read back**, so the pin kept the map alive while the local kept its pre-GC address — and `thread` |
| `posix_permission_bits_from_set` | `set`, across nine `Set.contains` dispatches |
| `exchanger_do_exchange` | `this` in the INNER loop; the outer re-read does not reach the park |
| `mirror_loaded_entries_to_properties_backend` | `this` and `chm` — same file, same shape as the four gdb caught |
| `walk_imports_recursive` | `registry`, across every `@Import` target and every recursion depth |
| `native_process_wait_for_timeout` | `this` across `foreign_exit_value` — `end_blocking_region_refs` refreshes it across the SLEEP and nothing refreshed it across the application bytecode the comment deliberately keeps outside the blocked region |

Two of those are worth singling out because the earlier pass named them as
survivors and they were not.

* **"the name is not an `ObjectRef` at all (`generic_type_mirrors` is a `Vec`)"**
  — true of the Vec and false of the defect. `pin_native_root` does not take a
  `Vec`; it takes an element. Pinning the elements is the fix.
* **"refresh via `end_blocking_region_refs`"** — true of the sleep and false of
  the loop. The same body calls `foreign_exit_value`, which the comment beside
  it says runs arbitrary application bytecode, and nothing refreshed `this`
  across that.

## The rule's own lower bound, unchanged

`--loops` requires the GC-capable statement to NAME the binding, so a reference
the body carries in and never mentions in a GC-capable statement is not
reported. That caveat stands. Where a row's fix would have been cosmetic
without it — `pd_gather_fold`'s `folder`, `ucp_init_2`'s `arr`,
`native_p64_ll_reversed`'s gathered elements — the whole loop was pinned rather
than just the reported name. Fixing half a loop leaves the defect.

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

**One of the 25 is proven; the other 24 are not.** The proof above is
`native_executable_get_annotated_parameter_types` and nothing else — every
other section of the probe passes on BOTH binaries, which is the expected
result for a window a few hundred bytes of allocation wide that no workload
aims at. It is the same flatness the four 2026-09-06 fixes' own A/B showed (7
vs 6 SIGSEGVs in 10).

What the proof changes is the standing of the other 24: they are no longer
"structurally identical to something that failed once, elsewhere". They are
structurally identical to something that fails HERE, on this branch's parent,
deterministically, with a wrong answer rather than a crash. Fixing a
loop-carried stale receiver is right whether or not a workload currently
reaches it; that argument now has a measurement behind it.

## Gates

* `--loops` over `native-{builtins,collections,io,api}`: **0 rows**.
* Calibration at `80db5d314^`/`80db5d314`: the two `native_properties_put_all`
  rows reported before the fix, gone after.
* Rule 1's own calibration is unchanged by every edit here: `native_fcimpl_open`
  reports 6 rows at `3950eed48^` and 0 at `3950eed48`.
* `cargo test -p cratonvm-native-builtins -p cratonvm-native-collections -p cratonvm-native-io`.
* `cargo clippy --all-targets` clean; `cargo check --features synthetic-jdk` clean.
* `regression-suite/run.sh` — **92 of 92 passed, 0 failed.**
* `probes/NativeLoopReceiverSweep.java` — byte-identical to HotSpot 25 under
  `--XX:UseGc Generational` and `--XX:UseGc G1`, `-Xmx64m`, and 5/5 under every
  stress configuration in the table above.

## One residual, found by the probe and NOT caused by this branch

The probe's `growth` section SIGSEGVs under
`GC_STRESS=65536 + DBG_FORCE_MOVING + DBG_STALE_OBJREF` together — 0/3, at
minor cycle 460 every run — and does so IDENTICALLY on `origin/dev` and on this
branch. Removing any one of the three flags makes it pass. It has its own page:
`docs/known-issues/gcprobes-stale-value-reaches-set_field-under-the-three-flag-harness-20260908.md`.
