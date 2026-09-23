# Runtime-helper proposals — round 10, lane `help2`

**Status:** PROPOSALS ONLY. Written at the close of a hard review of
`vm/src/jit/helpers.rs`, `vm/src/jit/helper_guard.rs` and
`jit-api/src/helpers_abi.rs` — completing a sweep an earlier `help` lane began
and lost to a rate limit after two findings. Nothing below was built or
measured; each item states its own cost. Two real defects were found and fixed
in-lane (see the round report): `jit_monitor_enter`/`jit_monitor_exit`'s
"no live JIT thread" arms were missing `set_jit_deopt_pending()`, and
`JIT_TYPECHECK_ANSWER_CACHE`/`JIT_SUBTYPE_POSITIVE_CACHE` had no
`class_definition_epoch` stamp at all, unlike every sibling memo in the same
file. The proposals below are about stopping this bug *class* from recurring,
not about either individual fix.

## 1. A shared `EpochStampedMemo<K>` so a positive type-check/subtype cache
   cannot be added without a validity stamp

**Where:** `vm/src/jit/helpers.rs` — `JIT_TYPECHECK_TARGET_CACHE`,
`JIT_TYPECHECK_ANSWER_CACHE`, `JIT_TYPECHECK_NEGATIVE_CACHE` and
`JIT_SUBTYPE_POSITIVE_CACHE` (four thread-locals, three shapes) plus every
`*_cached`/`*_cache_put` pair (`jit_typecheck_target_cached`/`_put`,
`jit_typecheck_answer_cached`/`_put`, `jit_typecheck_negative_cached`/`_put`,
and `jit_is_subclass_of_cached`'s inlined version of the same thing).

**What is true today.** Four thread-local caches in this file all answer the
same underlying question — "is this fact, proven about a bare `ClassId`
number, still true?" — and three different people apparently arrived at the
answer independently, twice getting it right (`JIT_TYPECHECK_TARGET_CACHE`,
`JIT_TYPECHECK_NEGATIVE_CACHE`, both `(u64, Vec<..>)` stamped with
`class_definition_epoch`) and twice getting it wrong (`JIT_TYPECHECK_ANSWER_CACHE`,
`JIT_SUBTYPE_POSITIVE_CACHE`, both bare `Vec<..>` with no stamp at all, fixed
this round). The failure mode in both wrong cases was the same reasoning
error, made independently: "this fact can never become false once true, so it
never needs to be invalidated" — true of the FACT (a hierarchy relationship,
fixed at class definition) but false of the CACHE ENTRY, because the entry's
key is a `ClassId` *number*, and numbers get recycled by unload. Nothing in
the code shape distinguishes a "this doesn't need a stamp" cache from a "this
does" one — both look like `RefCell<Vec<(usize, ..., u32, ...)>>` with a
linear scan and LRU eviction. A reviewer (or an author under deadline) has to
independently rediscover the `ClassId`-reuse hazard for every new cache of
this shape, and this round's evidence is that this is a coin flip, not a
review step people are reliably applying.

**Proposal.** A small generic wrapper —

```rust
struct EpochStampedMemo<K: PartialEq, const CAP: usize> {
    epoch: u64,
    entries: Vec<K>,
}

impl<K: PartialEq + Copy, const CAP: usize> EpochStampedMemo<K, CAP> {
    fn probe(&mut self, key: K, epoch: u64) -> bool { .. }   // clears+restamps on mismatch, else move-to-front scan
    fn record(&mut self, key: K, epoch: u64) { .. }          // clears+restamps on mismatch, else LRU-insert
}
```

— used for all four tables (the two array-shaped keys and the two
struct-shaped `ClassId` pairs both fit `K: PartialEq + Copy`). A cache that
uses this type gets the epoch stamp for free and cannot forget it; a future
author who reaches for `RefCell<Vec<..>>` directly instead is visibly doing
something unusual, which is the point — the wrapper turns "did you remember
the stamp" into "did you use the standard building block", which is a much
easier review question and a much easier thing to grep for
(`RefCell<Vec<(` outside this module would become a smell).

**Why it is worth doing.** This round found the SAME bug shape twice in one
file by hand; the earlier `help` lane's write-up (see `MEMORY.md`/the round
report) found the `JIT_TYPECHECK_TARGET_CACHE` instance of it in an earlier
pass. Three-for-three on one file strongly suggests the pattern recurs outside
it too (see item 2), and a shared type is cheaper than a fourth independent
rediscovery.

**Cost.** Small to moderate. The generic has to accommodate two current
shapes — `Vec<(usize, usize, usize, u32, bool)>` (typecheck target/answer/
negative) and `Vec<(usize, u32, u32)>` (subtype) — plus two current CAPs (16,
32, 64), which `const CAP: usize` covers. The main risk is perf: the current
code inlines the scan-and-swap directly in each function, and a generic would
need `#[inline]` discipline to keep the same codegen (these are hot paths —
the type-check answer cache's own doc measures 4.5% of a benchmark phase
riding on this exact loop). Should ship with the same before/after benchmark
the original caches were justified with, not assumed free.

## 2. Sweep sibling dispatch/native caches elsewhere in the tree for the same
   omission

**Where:** at minimum `vm/src/jit/helpers.rs`'s own `DISPATCH_CACHE`,
`VIRTUAL_DISPATCH_CACHE`, `OBJECT_NATIVE_DISPATCH_CACHE`,
`INTEGER_NATIVE_DISPATCH_CACHE`, `NATIVE_SITE_CACHE`, `VIRTUAL_TARGET_CACHE`,
`STATIC_BYTECODE_CALLEE_CACHE`, `VIRTUAL_BYTECODE_CALLEE_CACHE`,
`SPECIAL_BYTECODE_CALLEE_CACHE` (all already covered by
`flush_class_identity_dispatch_memos`'s single `(epoch, any_class_redefined)`
gate — checked this round, not a repeat finding), but the search should not
stop at this file. Any other `thread_local!` or process-global cache anywhere
in `vm/src`, `classloading/src`, `jit/src` or `native-builtins/src` that is
keyed even partly by a bare `ClassId`/`u32` class-id number and answers
`true`/a resolved id "for the life of the process" is a candidate.

**What is true today.** This lane's ownership (`vm/src/jit/helpers.rs`,
`helper_guard.rs`, `jit-api/src/helpers_abi.rs`) is a small fraction of the
places a `ClassId`-keyed memo could live. `is_subclass_of` in
`classloading/src/class_manager.rs` is itself the function
`JIT_SUBTYPE_POSITIVE_CACHE` wraps to avoid calling — a hierarchy-walk memo is
exactly the shape that shows up wherever a hot path wants to avoid a
class-manager lock, and this file is unlikely to be the only place that
pattern was reached for independently under perf pressure.

**Proposal.** A follow-up lane (or a scripted grep pass, since the shape is
mechanical: a `thread_local!`/`static` cache whose value type or key type
contains `ClassId`, `class_id`, or a raw `u32` documented as a class id,
cross-referenced against whether `class_definition_epoch` appears anywhere in
the same block) across the crates listed above. Each hit gets the same
question this round asked: does a `true`/resolved answer survive an unload
that reassigns the id, and if the code's own reasoning says "yes because the
fact is permanent", is that reasoning about the fact or about the numeric key.

**Why it is worth doing.** A false positive here is not a performance bug —
it is `checkcast`/`instanceof` (or whatever the analogous cache serves)
answering `true` for a class that was never checked, i.e. silent type
confusion in JIT-compiled code. That is the class of bug this round's charter
(sentinel discipline, GC safety, stale caches) exists to find, and the base
rate found in one file this round (two hits alongside two already-correct
siblings) does not support assuming the rest of the tree is clean.

**Cost.** Read-only investigation is cheap; each individual fix (once found)
is the same small, mechanical shape as this round's two — add a `u64` stamp,
gate the read and the write on it — so the expensive part is the search, not
the fix.

## 3. Symmetry check: does the outer `jit_bridge` top-level gate really apply
   uniformly, or did `emit_monitor_stub`'s doc get ahead of the code?

**Where:** `jit/src/runtime_lowering.rs` (`emit_monitor_stub`'s doc, "the
sentinel IS load-bearing, on both tiers"), `vm/src/runtime/interpreter/jit_bridge.rs`
(the `result == i64::MIN && deopt_signaled` gate, several call sites — none of
these files are owned by this lane). This lane fixed
`jit_monitor_enter_body`/`jit_monitor_exit_body`'s missing
`set_jit_deopt_pending()` (this file, `vm/src/jit/helpers.rs`) by reading
`jit_bridge.rs`'s actual gate rather than trusting the "int/ref/void returns
are unambiguous" claim elsewhere in this same file's own doc comments (which
describes a DIFFERENT, narrower, nested-call-site check, not the outer
`jit_bridge` boundary this fix targets — the two are easy to conflate, and
this lane initially did).

**What is true today.** The doc comment on `jit_dispatch_threw`
(`vm/src/jit/helpers.rs`, "For `int`/ref/void returns that is unambiguous...")
and the doc comment on `emit_monitor_stub` (`jit/src/runtime_lowering.rs`,
"the sentinel IS load-bearing... a null receiver takes the NPE path") each
describe true facts about DIFFERENT checks — an inline nested-call-site
`CMP`+jump inside a compiled caller vs. the outer `jit_bridge` post-invoke
gate reached when the WHOLE compiled method returns to the interpreter — but
neither says so explicitly, and nothing marks which one a given piece of
commentary is about. This round needed to read `jit_bridge.rs`'s actual code
(not owned by this lane) to resolve the ambiguity for the monitor fix.

**Proposal.** A short cross-reference note — in `helper_guard.rs`'s module
doc (this lane's file) and/or `jit_dispatch_threw`'s doc (also this lane's
file) — stating explicitly that TWO distinct sentinel checks exist in this
codebase (the inline nested-call-site check, type-specific and sometimes
unconditional; and the outer `jit_bridge` top-level gate, which per
`jit_bridge.rs`'s "MEDIUM fix" comment applies `result == i64::MIN &&
deopt_signaled` uniformly regardless of return type), and that every "no live
JIT thread" / "should be unreachable" early-return arm in a `Throw`-policy
helper must satisfy the OUTER gate — i.e. must call `set_jit_deopt_pending()`
— because that is the one every `i64::MIN` sentinel eventually reaches once
control unwinds all the way to the interpreter, regardless of which inline
check (if any) it passed through on the way. This would have made the
monitor-helper defect (and the two already-fixed twins it was copied from,
`native_integer_value_of`/`native_long_value_of`) mechanically checkable
instead of individually rediscovered three times.

**Why it is worth doing.** This round is evidence the same defect recurs
piecemeal — `native_integer_value_of`, `native_long_value_of`, and now
`jit_monitor_enter`/`jit_monitor_exit` all independently needed the identical
one-line fix for the identical reason. A single sentence naming the invariant
in one place (rather than three near-identical inline comments, now four)
would let a future audit check "does every `Throw`-policy helper's
unreachable-thread arm call `set_jit_deopt_pending()`" as one grep instead of
re-deriving the reasoning per call site.

**Cost.** Documentation only; no code or behavior change. The one thing worth
pairing with it (not proposed here, since it touches a file this lane does
not own) is a `debug_assert!` inside `jit_thread_mut()`'s `None` arm's
callers — infeasible as a blanket check since not every `None` arm returns
the sentinel (some return `-1`, `0`, `f64::NAN.to_bits()`, or `None`, all
correctly documented in this file as unambiguous without the flag) — so this
stays a documentation proposal, not a mechanical gate, unless a future lane
finds a cheap way to distinguish "this arm returns the `i64::MIN` sentinel"
from "this arm returns some other unambiguous failure value" at compile time.

## 4. `jit_indy_bridge`'s silent-null "no live JIT thread" arm is the one
   remaining sibling this round chose not to touch

**Where:** `vm/src/jit/helpers.rs`, `jit_indy_bridge_body`, the
`jit_thread_mut() else` arm (self-documented in place: "UNCONDITIONAL. This is
a should-never-happen that answers with a NULL REFERENCE, which is the worst
shape a helper can have... inherited from the concat bridge, where it has
always been silent").

**What is true today.** Unlike the monitor helpers, this arm does NOT return
the `i64::MIN` sentinel — it returns a bare `0` (null reference), which is
already a legitimate value for an `invokedynamic` call site (many bootstrap
methods can legitimately produce `null`), so `set_jit_deopt_pending()` would
not fix anything here and was correctly left alone this round; the defect (if
it is reached at all, which every piece of surrounding commentary treats as
effectively impossible) is a silent wrong VALUE, not a sentinel-discipline
gap. This was reviewed and explicitly NOT treated as a repeat of this round's
main finding — noted here so a future reviewer does not re-flag it as the
same bug class and does not assume this round missed it.

**Proposal.** Out of scope for a mechanical sentinel-discipline fix; if this
is ever worth closing, it needs a caller-visible way to distinguish
"legitimately null" from "helper environment failure", e.g. threading a
distinct out-of-band flag the way the `i64::MIN` sentinel does today, which
is a larger design change than this round's charter covers.

**Cost.** N/A — not proposing an implementation, only recording the triage
so the finding is not lost between rounds.
