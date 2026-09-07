# An inline splice's return value can land on an ENCLOSING spliced callee's live local

| | |
|---|---|
| **Status** | **FIXED** 2026-09-07. `Compiler::open_inline_locals_floor`, enforced in `reserve_spill_slots`. |
| **Origin** | `jit-warm-groupdata-window-row-collapse-20260906-FIXED.md` left a bare count of 325 spill overlaps as "that hazard is real and deserves its own page". The page that produced classified the count; this is that page with the fix. |
| **Never observed to produce a wrong answer.** | The workload that exposes it is 11/11 before and after. What is fixed is the hazard, not a failing test. |
| **The hazard is worse than this page originally said.** | 147 of the 151 overlaps land on a word the enclosing splice publishes to the collector as a **rewritable GC root**, on the DEFAULT configuration. See "How bad each of the 151 was". |

## The defect

A spliced callee's locals are reserved once, at its `callee_local_base`, and
stay live until the wrapper pops the scope. They are **not on the operand
stack**. Every "is this slot still owned?" scan in the compiler asks the operand
stack:

* `pop_stack`'s reclaim arm scans `self.stack`;
* `reset_spills` scans `self.stack`;
* the live-slot clamp in `try_emit_inline_body` scans `self.stack`.

So any of the ~70 places that assign `next_spill_offset` can leave the cursor
pointing inside an enclosing splice's locals, and the next reservation hands
that word out to a second owner. The enclosing callee's body continues after the
inner call returns; it then reads back whatever the new owner stored.

`dbg_note_spill_overlap` (`CRATONVM_DBG=jit-slot-overlap`) on
`CriteriaWindowFunctionTest`:

| | before | after |
|---|---:|---:|
| overlap reports | 303 | 157 |
| ...INNERMOST (harmless) | 152 | 157 |
| ...**ENCLOSING (the hazard)** | **151** | **0** |
| `@@RESULT` | `found=11 ok=11 failed=0` | `found=11 ok=11 failed=0` |

Same binary, one switch (`CRATONVM_JIT_NO_INLINE_LOCALS_FLOOR=1` is the A arm).
`inline_locals_floor_bumps=516` — the guard engaged; a zero there would mean the
result above was credited to something that never ran.

## The fix

```rust
// jit/src/x64/operand_stack.rs
pub(super) fn open_inline_locals_floor(&self) -> i32 {
    let enclosing = self.inline_oop_scopes.len().saturating_sub(1);
    self.inline_oop_scopes.iter().take(enclosing)
        .map(|s| s.local_base + (s.num_locals as i32) * 8)
        .max().unwrap_or(i32::MIN)
}
```

applied in `reserve_spill_slots`, which raises `start` to the floor before
handing a slot out.

### Why at the RESERVATION and not at the descent

**This page's own recommendation was wrong, and the measurement is how that was
found.** It proposed extending `try_emit_inline_body`'s live-slot clamp to cover
open scopes' locals — "a two-line change" — and predicted that would settle it.

Tried first. It engaged **3 times** on the workload while all 150 reports
stayed. The cursor does not reach an enclosing scope's locals through the
splice's return-value reclaim.

Tried second: guard the two obvious descent paths, `pop_stack`'s reclaim arm and
`reset_spills`. That took 150 → **125**. Better, and still not the mechanism —
there are a dozen more assignments to `next_spill_offset` inside `inlining.rs`
alone (a nested real call's `post_pop_spill`, the merge-point `save_spill`
restore, each bail path), and any one of them can leave the cursor low.

So the floor is enforced where a word is actually **handed out**, which is the
one choke point every descent path funnels through — and is where
`dbg_note_spill_overlap` already watches, which is why the detector can now
prove the population is empty rather than merely smaller. A cursor left below
the floor is harmless until something reserves; the next reservation bumps past
the open locals and `next_spill_offset` self-heals above them.

### Why the INNERMOST scope is excluded

The same split the detector reports. The innermost scope is the splice that is
RETURNING: its `xreturn` arm has already loaded the value into RAX and the
wrapper pops the scope a few lines later, so its locals are dead at that
instruction, and the result landing on its own local 0 is exactly what
`caller_post_pop_spill == callee_local_base` means.

Including it is not merely wasteful, it is **wrong**, and the tree already had
the test that says so. The first cut of this floor covered every open scope and
turned `a_spliced_callees_result_lands_at_the_callers_operand_depth` red: a
spliced callee's result must land at the caller's operand depth, and pushing it
above the callee's locals instead is the ECJ
`OperandStack.pop(OperandCategory)` miscompile recorded in the `xreturn` arm's
own comments — the one where the value landed two slots deep and every JSP
compiled afterwards threw `AssertionError: Unexpected operand at stack top`.

Nothing is lost by the exclusion: a reservation cannot reach the innermost
callee's locals from inside its own body. That callee's operands start at
`save_spill`, which is above `merge_base`, which is above its locals, and
`pop_stack` can only unwind slots something actually pushed. The one path that
reaches them is the return reclaim — the harmless one. The 157 INNERMOST reports
that remain after the fix are that path, and they are correct.

## Cost

None measured. Same workload, same binary, one switch:

| | floor OFF | floor ON |
|---|---:|---:|
| `c1` / `c2` compiles | 620 / 358 | 617 / 357 |
| `code_buffer_bails` | 1 | 1 |
| wall | 15.5 s, 16.4 s | 13.5 s, 15.5 s |

The theoretical cost is a delayed reclaim — a frame word left alone while a
splice that owns it is open — which is the same trade `pop_stack`'s own
live-slot scan documents, and is bounded the same way: `checked_spill_range_end`
fails the compile and falls back to the interpreter rather than emitting a wrong
body. No compile hit that bound.

## Why nothing was visibly broken

The workload was 11/11 throughout. The open page offered two reasons it can be
quiet; the measurement below retires the first and leaves the second:

* ~~the clobbered local may be dead from that point in the enclosing body~~ —
  **retired.** Deadness is no defence against the GC channel: the oop map names
  the slot because the dataflow says it holds a reference, not because anything
  reads it. See "How bad each of the 151 was".
* the value written may be the same object the local held, when the inner call
  returns its own receiver — very common in bytebuddy's `describe`/`of`/`wrap`
  chains, which is where 39 of the 44 affected methods came from. This one
  stands, and is what the workload was actually relying on.

Neither was enforced anywhere, so it was luck. That is what changed: the
invariant is now enforced rather than hoped for, and the detector proves the
population is empty.

## The witness

`x64::tests::a_reservation_never_starts_inside_an_open_inline_scopes_locals`.

Three open scopes, the `#1/3` shape the census reports — two would leave a
single enclosing scope and pass just as well against a `last()`-style floor that
ignores the outer one. It asserts the floor is the max over the ENCLOSING scopes,
that a reservation from a cursor below them is bumped past them, that the cursor
comes back above them, and that a lone scope and no scope are both inert.

It is load-bearing: under `CRATONVM_JIT_NO_INLINE_LOCALS_FLOOR=1` it fails with
`reservation was handed slot 40, inside the locals of an open enclosing inline
scope (40..80)`.

It asserts on the RESERVATION rather than on a computed answer, for the reason
its sibling `a_splice_does_not_rewind_the_cursor_under_a_buried_operand` gives:
the defect lives in the spill-slot simulation, and the descent that reaches an
enclosing scope's locals needs a three-deep splice in an 11 KB bytebuddy body to
occur naturally.

## How bad each of the 151 was

The open page asked for this and called it Option 1: use `InlineOopScope`'s
per-pc local oop masks to turn "151 maybe" into a number of "definitely".
`dbg_note_spill_overlap` now does it, and reports it per overlap as
`oop-root-after=`. Measured against the PRE-fix arm
(`CRATONVM_JIT_NO_INLINE_LOCALS_FLOOR=1`), which is why the switch exists:

| of the 151 ENCLOSING overlaps | |
|---|---:|
| `oop-root-after=yes` | **147** |
| `oop-root-after=no` | 4 |
| `oop-root-after=unknown` | 0 |

`yes` means: at some REACHABLE pc at or after the point the enclosing scope is
stopped at, its must-be-oop dataflow has the overlapped local's bit set. That is
not "the callee reads it again" — it is stronger and worse. `collect_live_oop_homes`
publishes exactly `scope.local_base + k*8` as a `ShadowHome::Frame` for every set
bit, so the word is handed to the collector as a **rewritable reference root**:
relocation reads it as a pointer and writes the new address back.

So a second owner storing a primitive into that word is not merely a wrong value
in the enclosing callee. It is a non-pointer handed to the collector as a root,
whose relocation then also corrupts the second owner's value. And this is the
DEFAULT path, not a corner: `GcFlags::moving_young` is default-on
(`CRATONVM_NO_MOVING_YOUNG` is the opt-out) and `complete` in
`collect_live_oop_homes` is exactly that flag.

**This retires the first of the two "why nothing is visibly broken"
explanations.** That bullet read "the clobbered local may be dead from that
point in the enclosing body (the common case for a `num_locals=1` scope whose
single local is `this`)". Deadness is no defence against this channel: the oop
map names the slot because the dataflow says it holds a reference, whether or
not any Java bytecode reads it again. A `num_locals=1` scope holding `this` is
the WORST case here, not the most benign one — `this` is a must-oop at every pc,
which is why 147 of 151 answer `yes`.

The second explanation stands and is what this workload was actually relying
on: bytebuddy's `describe`/`of`/`wrap` chains return their own receiver, so the
word usually held a valid oop either way.

### What this still does not prove

A witness. `yes` says a later reachable pc names the local; it does not say a
safepoint was emitted at such a pc after the clobber, nor that the value stored
by the second owner was a primitive. Both are needed for an actual miscompile,
and neither is measured. What changed is the size of the gap: the hazard is no
longer "the enclosing callee might read a wrong value", it is "the collector may
be handed a non-pointer as a rewritable root", 147 times in a 20-second run, on
the default configuration.

The pre-fix arm remains reachable through the switch, so anyone who wants the
witness can go after it with the instrument already in place.

## Related

* `docs/internal/fixed-bugs/jit-warm-groupdata-window-row-collapse-20260906-FIXED.md`
  — where the count came from, and the defect it was NOT.
* The `LEATest` miscompile recorded in `try_emit_inline_body`'s own comments —
  the same class of defect on the operand stack, fixed by the clamp that this
  page's first attempt tried and failed to extend.
* The ECJ `OperandStack.pop` miscompile in the `xreturn` arm's comments — why
  the innermost scope is excluded.
