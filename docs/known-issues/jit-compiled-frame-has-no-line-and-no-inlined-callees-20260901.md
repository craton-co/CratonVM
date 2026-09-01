# A JIT-compiled frame has no line number and no inlined callees, so a warmed-up stack trace is wrong

**Status:** OPEN. Reproduced 2026-09-01 on `dev` @ `56d6c3722`, x86-64 Linux,
default flags, real-JDK mode, JDK 25.

**Severity:** high for diagnosability, zero for program results. Every trace
degrades silently and only *after* warm-up — that is, only in the runs anyone
cares about. Nothing throws, nothing logs, and the trace looks plausible.

## The one-run witness

`probes/StackTraceAfterOsr.java` throws from the *same* site three times in one
process. Only the amount of prior warm-up differs.

```
                   HotSpot 25 (-XX:-OmitStackTraceInFastThrow)  CratonVM (default)
before_any_warm    len=5 [leaf:25 mid:26 outer:27 probe:42 main:54]  len=5 [leaf:25 mid:26 outer:27 probe:42 main:54]
after_helper_warm  len=5 [leaf:25 mid:26 outer:27 probe:42 main:58]  len=5 [leaf:25 mid:26 outer:27 probe:42 main:58]
after_main_osr     len=5 [leaf:25 mid:26 outer:27 probe:42 main:66]  len=3 [leaf:25 probe:-1           main:62]
```

The third row is the defect, and it is three defects stacked:

1. **`probe:-1`** — a compiled frame carries **no line number at all**.
2. **`mid` and `outer` are gone** — they were inlined into `probe`'s artifact,
   and an inlined callee contributes no frame.
3. **`main:62` instead of `main:66`** — 62 is `main`'s OSR back-edge, 66 is the
   actual call site. Once `main` OSR-enters, its interpreter `Frame.pc` stops
   advancing and every later trace reports the loop it tiered up in.

The NPE detail message is lost on the same row: HotSpot says
`Cannot load from int array because "StProbe.table[...]" is null`, CratonVM's
interpreter says `Cannot load from int array` (the ` because …` clause is never
built), and the compiled path says `null`.

`CRATONVM_DISABLE_JIT=1` restores all five frames, the correct lines and the
message, on the same binary and the same `.class` file. That is the A/B.

## Where each one lives

**(1) is a hard-coded constant, not a resolution failure.**
`vm/src/runtime/stackwalker.rs`, `compiled_frame_entry`:

```rust
// No bytecode index is recorded for a compiled frame, so there is no
// line to resolve. Never guess one: a wrong line is worse than none.
line_number: LINE_NUMBER_UNKNOWN,
byte_code_index: -1,
```

The reasoning is right and the premise is stale. The JIT *does* record a bci
per site — `bytecode_pc` is the per-site metadata key that inline caches and
deopt resume points are both built on, and `deopt.rs` reconstructs a full
interpreter frame from a compiled PC for every deoptimisation. The bci exists;
it is simply not carried on the `(depth, label, class_id, cm_ptr)` tuple that
`conservative_roots::active_compiled_frames()` returns, and that tuple is the
only thing `compiled_frame_entry` is given. Widening the tuple to carry the
current bci — the same one a deopt at that PC would resume at — makes
`resolve_line_numbers_in_place`'s existing machinery work unchanged.

**(2) is a shape problem in the same tuple.** `active_compiled_frames()` is
flat: one entry per compiled artifact. HotSpot's equivalent is a `ScopeDesc`
*chain* — an inlined callee is a nested scope on the same PC, which is exactly
what makes `leaf`/`mid`/`outer` reappear. The information again exists in the
tree: `InlineSite::nested_sites` is already a recursive structure and the
sizing code in `x64/driver.rs` already walks it transitively. What is missing
is a PC→(inline chain, bci per level) map emitted alongside the oop map.

**(3) is the interpreter frame going stale under OSR.** `drop_osr_continuations`
was written for the *duplicate* frame this produces and is not the fix for the
stale one — verified: `CRATONVM_JIT_NO_OSR_FRAME_DEDUPE=1` changes nothing on
this witness. The frame's `pc` needs to be refreshed from the compiled
activation before a trace is captured, the way deopt already refreshes it.

`CRATONVM_JIT_NO_INLINE=1` isolates (1) from (2) and exposes a fourth,
smaller issue — the compiled frame is emitted *in addition to* its interpreter
frame rather than instead of it:

```
CRATONVM_JIT_NO_INLINE=1  →  hot=... leaf:9 leaf:-1 mid:-1 outer:-1 main:34
```

`leaf` appears twice. `drop_osr_continuations` only dedupes OSR continuations,
not ordinary compiled-call activations.

## Why this matters more than it looks

Every logging framework, every `catch (Exception e) { log.error("…", e); }`,
every Spring/Hibernate/Jackson diagnostic and every crash report reads
`getStackTrace()`. On a cold path they are correct. On a hot path — the one a
production incident is about — they name the wrong line, omit the three frames
that would have identified the caller, and drop the NPE's explanatory clause.
An operator reading such a trace is not told that anything is missing.

It also silently weakens two things inside the VM that read the same walk:
`resolve_caller_class_id` (the JEP 403 deep-reflection gate) and the
`Class.forName` caller-loader lookup already had to grow
`frame_class_ids_with_compiled` for the *frame* half of this problem; the *bci*
half is still missing there too.

## What would close it

In dependency order, smallest first:

1. Carry the current bci on the compiled-frame tuple and drop the hard-coded
   `LINE_NUMBER_UNKNOWN`. Closes (1). No new metadata — deopt already computes
   this mapping.
2. Refresh the interpreter `Frame.pc` of an OSR-entered activation before a
   capture. Closes (3).
3. Dedupe an ordinary compiled activation against its interpreter frame the way
   OSR continuations are deduped. Closes (4).
4. Emit a PC→inline-chain map beside the oop map and expand it during capture.
   Closes (2). This is the only one that is real work.

Build the `because "…" is null` clause for the interpreter's NPE messages
separately; it is unrelated to the JIT and is a pure `native-builtins` fidelity
gap.

## Reproducer

`probes/StackTraceAfterOsr.java`. Run all three arms:

```sh
java -XX:-OmitStackTraceInFastThrow -cp probes StackTraceAfterOsr
cratonvm -cp probes StackTraceAfterOsr
CRATONVM_DISABLE_JIT=1 cratonvm -cp probes StackTraceAfterOsr
```

The first and third agree; the second is short by two frames with two wrong
line numbers.
