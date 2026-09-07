# `native-io` unpinned-native-local audit — swept, including the 95% the default rule could not see

| | |
|---|---|
| **Status** | Swept. Three tranches read: 24 parameter rows, 46 local rows, 108 `--any-binding` rows. **154 pin/re-read pairs added across ~140 bindings**; 41 rows classified as false positives. The audit reports 4 by default and 25 with `--any-binding`, every one of them named below. |
| **Scope** | `native-io/src` — the crate the `unpinned-native-locals-audit-48-fixed-20260825` pass never covered. |
| **Tool** | `scripts/unpinned-native-local-audit.py` |

## The gate that hid most of it

Rule 1 required the BINDING's own statement to be GC-capable. That is a proxy
for "this local names a fresh object", not the defect's definition, and the
control priced it exactly: at the commit before the FileChannelImpl fix,
`native_fcimpl_open` held seven references that fix had to root, and the rule
reported **six**. `fd_obj`, bound by `match args.first()`, was invisible because
`args.first()` does not allocate — and that is the shape of the one instance of
this family anyone has observed failing.

`--any-binding` drops the requirement, keeps the type filter, and reports 7 of 7
on that control with the post-fix file still silent. On `native-io` it reported
**139 against the default's 7**.

Two rule corrections then took 139 to **108**, and both came from reading the
rows rather than from theory:

* **A GC on a path that LEAVES does not precede what follows it.** Ten
  `socket_channel.rs` rows had latched onto
  `let Some(id) = reg_id else { return Err(closed_channel_exception(ctx)); };`
  and reported a window that cannot exist — hiding whichever later call is the
  real one. `leaves()` now skips those and keeps looking. This is not a
  branch-exclusivity guess: the statements below are reached only when that
  branch did not run. A `match` arm is deliberately NOT included.
* **The scalar test has to key on the destructuring ARM.** Looking for any
  `Value::Int(` and no `Value::Object` anywhere in the statement is useless,
  because the call's own ARGUMENTS carry `Value::Object(Some(buf))`:
  `let n = match ctx.invoke_virtual(inner, "read", "([BII)I", &[Value::Object(Some(buf)), …]) { Ok(Some(Value::Int(n))) => n, … }`
  binds an `i32` and mentions both in one breath.

## 108 read, 94 fixes applied, 25 rows left standing

Every row was read through a triage dump printing the three statements that
make it one — binding, intervening GC, use — which is enough to type-triage
most rows without opening the file.

**83 insertions were applied mechanically** (73 on the first pass, 7 on the
second, 3 on the third). The shape is uniform enough to automate: a
tool inserts `pin_native_root` after the binding and `read_native_pin` before
the use, and nothing else. No unpin — every function it was allowed to touch is
a native entry point, and `safe_native_call` truncates the pin stack to its
entry floor on return. It was run **to a fixpoint**: the shadowing `let` before
the first use also covers later uses in that block, so applying, re-running the
audit and applying again closes the second and third windows. Three passes.

The tool decides nothing. An allowlist written by hand after reading decides,
and the two compile errors it produced (a shadow that dropped a `mut`) were
fixed by hand.

**11 were fixed by hand** — the ones where a pin already existed and the
re-read was one statement too early (`scan_make_pattern` releases `source`
before the allocation that follows), where a second reference crossed the same
window (`native_bos_write_bulk_locked`'s `src` alongside its `this`), or where
the function is a helper that needs its pin released (`encode_isa`,
`write_through`, `flush_pending_surrogate`, `read_process_redirects`).

## Three functions were already right, and the rule cannot tell

`native_process_wait_for`, `native_process_on_exit` and the three `pipe.rs`
channel operations all do

```rust
let mut held = [Value::Object(Some(buf))];
ctx.begin_blocking_region();
…
ctx.end_blocking_region_refs(&mut held);
let buf = match held[0] { Value::Object(Some(o)) => o, _ => buf };
```

which is the correct idiom — and the refresh goes through an ARRAY, not through
the name, so the scan cannot follow it. An automated pass inserted redundant
re-reads into two of them before this was noticed; that pass was reverted and
the functions added to the denylist. **A rule that cannot see a correct fix will
"fix" it again**, which is its own kind of damage.

## The 25 that remain, by cause

| cause | rows |
|---|---|
| the refresh goes through `end_blocking_region_refs`, not through the name | 7 |
| a Rust `String` / `Option<String>` bound from an `invoke_*` that returned an object (`object_to_string`, `read_string`) | 6 |
| an integer or `FdId` the arm pattern does not reveal | 2 |
| branch-exclusive: the only GC is on a path that returns, in a form `leaves()` deliberately does not decide (a `match` arm) | 8 |
| a torn `let x = match { arm => { let …; … } }`, where the allocation that BINDS `x` reads as one that follows it | 1 |
| `ensure_open`, which allocates only on the path that throws | 1 |

None of these is a defect. They are the residue of a rule that is deliberately
noisy rather than confidently dismissive, and the page names each one so the
next reader does not re-derive them.

## Calibration

* **Rule 1**: `native_fcimpl_open` before and after `3950eed48` — 7/7 with
  `--any-binding`, 0 after. Unchanged by every rule edit in this pass.
* **Rule 2**: 83 historical commits that added a pin for a declared
  `x: ObjectRef` parameter in `native-collections/src/lib.rs`, run at each
  commit and its parent — **precision 81/83, recall 58/77**, identical before
  and after this pass's filtering. The filtering cost no recall.

## What this does NOT establish

**No dynamic proof, and the same caveat the 2026-08-25 page carried.** These are
structurally identical to a defect that WAS proven — the `FileChannelImpl` the
Generational young sweep zeroed — and none was observed failing. Pinning across
a call that can move the object is right whether or not a workload currently
reaches it, but this is not "140 live bugs found".

What it does establish is that the crate is now swept by a rule whose reach is
measured rather than assumed, with every survivor named.
