# The JIT's `aastore` never called the helper that holds its store check

> **RECONCILED 2026-08-16 (merge of `dev`).** Every `aastore_check`,
> `jit_aastore_check` and `HelperFnAastoreCheck` below names the spelling that
> was current when this record was written. The merge of `dev` into
> `claude/jdk-only-mode-completion-1351c0` settled on **`aastore_type_check`**
> (ABI field), **`jit_aastore_type_check`** (helper) and
> **`aastore_store_is_refused`** (the predicate the helper and the x64 inline
> lowering now share), and the slot is `required: true` -- the old
> `aastore_check == 0` fallback that routed the whole opcode to
> `helpers.aastore` no longer exists. `grep -rn 'aastore_check' --include=*.rs`
> returns nothing in the tree. The names here are kept as written; read them as
> history, not as a pointer to live code.

**Status: FIXED IN SOURCE (this record's own commit), NOT YET EXECUTED.**
The code change is one arm of `jit/src/x64/bytecode_walk.rs`. No binary was
built or run in the wave that wrote this — hard constraint of the lane, not an
omission — so every "after" value below is marked **PREDICTED** and the
scheduled vector (§5) is what settles it. Do not retire this record on the
source diff.

Filed 2026-08-12. Predecessor: `W7-37-differential-throwable-and-vm.md` §"Part
4", which **diagnosed this defect correctly and completely** and did not fix it;
this record is the codegen change Part 4 named, plus the two-tier vector it
lacked.

---

## 1. What was wrong

`aastore` (opcode `0x53`) is the reference array store. JVMS §6.5 *aastore*
requires that the stored value be assignment-compatible with the array's
**actual runtime component type**, not its static type, and that the store throw
`ArrayStoreException` otherwise. This is the rule that makes Java's covariant
arrays safe:

```java
Object[] a = new String[1];
a[0] = Integer.valueOf(1);   // must throw ArrayStoreException
```

CratonVM had the check. It was in `jit_aastore`
(`vm/src/jit/helpers.rs`), correctly written, routed through
`throw_runtime_error` so the message carries HotSpot's external class name.

**The x64 JIT never called it.** `jit/src/x64/bytecode_walk.rs`'s `0x53` arm
lowered the opcode inline — null check, bounds check, SATB pre-write barrier,
`MOV QWORD [array + index*8 + HEADER_SIZE], val`, card mark — and never touched
`self.helpers.aastore`. So in compiled code the store simply happened.

## 2. How a correct check ended up unreachable

This is the part worth carrying forward, because nothing was careless.

The inline lowering was a deliberate throughput change (R20 / HIGH-5). It
replaced the `jit_aastore` call with the inline store, and it justified dropping
the call in a comment:

> "ArrayStoreException note: the current `jit_aastore` helper does NOT enforce
> the ASE check (the interpreter does it via `set_array_element`). This inline
> path matches the helper's behavior exactly — **no regression**."

Every clause of that was **true when it was written**. It was falsified later,
silently, when the covariance check landed *inside* `jit_aastore`. The helper
gained a check; the only caller that would have exercised it had already been
deleted; nothing failed to compile, because **a premise stated in a comment is
not a compile-time link**.

The two halves even document each other. The helper's own comment (added when
the check landed) says the arm "does not run at all, because nothing calls this
function", and the emitter's comment says the helper has no check. Each was
written by someone who had read the other at a different point in time, and both
were left in the tree simultaneously asserting incompatible things. **A
correctness invariant that lives as prose in two files will drift, and the drift
is invisible to the build.**

## 3. Why this is worse than a wrong answer

The illegal store completes. An `Integer` ends up inside a `String[]`.

Nothing detects it at the store, and nothing detects it at the load either: a
later `aaload` yields a reference the verifier and the compiler both believe is
a `String`, with no cast to fail. Under a **precise GC** the array's component
type participates in how its contents are traced and relocated, so this is heap
type confusion — a memory-safety-relevant corruption — rather than an
etiquette problem. The blast radius is whatever reads the array next, which is
arbitrarily far from the store that caused it.

**And it is tier-dependent, which is the expensive part.** The interpreter
enforced the check the whole time. So the program is *correct for the first ~500
executions of the method and wrong afterwards*, with no event in between that any
log records. In the field this presents as: works in tests, works on small
inputs, corrupts the heap under load — the exact profile that costs the most to
diagnose, because every reduction that makes it fast enough to reproduce also
makes it hot enough to tier up, and every reduction small enough to debug stays
interpreted and comes back green. A single-execution fixture is structurally
incapable of seeing it.

## 4. The fix

`jit/src/x64/bytecode_walk.rs`, the `0x53` arm: route the whole opcode to the
helper.

```rust
0x53 => {
    self.flush_scratch_registers();
    let val_slot = self.pop_stack();
    let index_slot = self.pop_stack();
    let array_slot = self.pop_stack();
    // jit_aastore(vm_ptr, array_ptr, index, val)
    self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
    self.load_slot_to_reg(ARG_REGS[1], array_slot);
    self.load_slot_to_reg(ARG_REGS[2], index_slot);
    self.load_slot_to_reg(ARG_REGS[3], val_slot);
    self.emit_call_absolute(self.helpers.aastore);
    self.emit_post_invoke_exception_check(b'V');
    pc += 1;
}
```

Four properties made this the right shape rather than a minimal patch:

* **`jit_aastore` is the complete opcode, not a check.** It does the null check
  (pending-NPE, `ASTORE_OBJECT` action), the bounds check (pending-AIOOBE with
  index and length), the covariance check, the SATB pre-write barrier, the
  store, and the card mark, in that order. Calling it restores all six; adding
  an ASE check to the inline path would have restored one.
* **No ABI change and no new helper.** `helpers.aastore` has been populated the
  whole time (`vm/src/jit/helpers.rs`, the `JitRuntimeHelpers` initializer); the
  slot was simply never emitted against. The fix is therefore landable on its
  own, in one crate — which matters, because the alternative shape below is not.
* **The operand loads cannot clobber each other.** `flush_scratch_registers`
  rewrites every register-resident (`Scratch`/`Xmm`) stack slot to a frame slot,
  so all three `load_slot_to_reg` calls read from memory. That holds for both
  ABIs (`ARG_REGS` is RCX/RDX/R8/R9 on Windows, RDI/RSI/RDX/RCX on SysV). The
  pre-existing arm relied on the same property.
* **`0x53` is a one-byte opcode**, so `emit_post_invoke_exception_check` keeps
  *this* pc as the throw pc, which is what the handler `[start_pc, end_pc)`
  range test requires.

**This is a throughput regression and should be recorded as one.** It pays back
one call per reference array store — precisely the cost R20 removed. The cheaper
shape is to keep the inline lowering and call a *check-only* helper before the
store; that needs a new helper plus a `jit-api` ABI slot, i.e. a coordinated
change across three crates. It is deliberately **not** bundled here, because the
correctness fix has to be landable without it. Nominated in §6.

### Single-site, verified

`0x53` has exactly one emission site. The other JIT tiers refuse the opcode
rather than lowering it, so there is no second path to fix and no possibility of
a tier disagreeing:

* `jit/src/ir_lower.rs` — `MemKind::Ref` array stores `latch_bailout` with
  "ir_lower: ArrayStore(Ref) needs the SATB + card write barriers". Fail closed.
* `jit/src/aarch64_backend.rs` — `0x53` is in `object_model_opcodes_are_all_unsupported`; the backend bails.
* `jit/src/x64/escape_analysis.rs:171` — analysis only, emits nothing.

`emit_ref_astore_regs` (`jit/src/x64/arrays.rs`) is left with no callers by this
change. That is not a build failure: `dead_code = "allow"` workspace-wide
(`Cargo.toml`, `[workspace.lints.rust]`). It is deliberately **not** deleted —
the check-only follow-up in §6 needs it back.

## 5. Vector

`regression-suite/src/RArrayStoreTiers.java` (new, this record). Sixteen store
shapes, each in its own method so each gets its own compiled site, operands read
from static fields at their widest type so javac must emit a real `aastore`
against a component type it cannot prove.

**It must be run twice — with and without `--nojit`** — and the two runs are not
redundant. A single green run cannot distinguish "both tiers are right" from
"the JIT never engaged". Red without `--nojit` and green with it localises the
defect to the compiled tier; red both ways means the shared check is wrong.

`ITERS = 3000`. The C1 threshold is 500 (`jit/src/tiered.rs`,
`CompilationPolicy::default`, override `CRATONVM_TIER_C1_THRESHOLD`), but
crossing it only *enqueues* the method on the background compile worker — the
compiled entry is installed some time later, so a fixture that stops at 600 can
finish before compiled code is ever entered and **read green on a broken VM**.
3000 leaves 2500 iterations of margin. The fixture prints the iteration at which
each answer moved; if that number ever approaches `ITERS`, raise `ITERS` rather
than trusting the result.

### The fixture was mutation-checked, and its first version was wrong

Emulating the defect in pure Java — refuse the store for 500 calls, then perform
it — makes the fixture go red with exactly the shape W7-37 Part 4 recorded:

```
s01 String[] as Object[] <- Integer   cold=[ArrayStoreException] hot=[no-throw]  MOVED@499
DIVERGENCE s01 ... HOT: want=[ArrayStoreException] got=[no-throw]
DIVERGENCE s01 ... TIER-SPLIT at i=499: cold=[ArrayStoreException] became=[no-throw] final=[no-throw]
```

Its **first** version compared the full exception *string* across tiers and went
**red on HotSpot itself**. That trap is recorded separately in
`W7-40-tier-parity-fixtures-and-fast-throw.md`; the short form is that the
message is not a tier-invariant and asserting it across tiers measures the
oracle's optimizer instead of the subject.

### Measurements

HotSpot 25.0.3 (Microsoft build 25.0.3+9-LTS), measured:

| run | result |
|---|---|
| `java RArrayStoreTiers` | `PASS RArrayStoreTiers (63 checks)` |
| `java -Xint RArrayStoreTiers` | `PASS RArrayStoreTiers (63 checks)` |
| mutant (defect emulated) | `AssertionError: 2 divergence(s)`, `MOVED@499` |

CratonVM, before: **not run this wave.** W7-37 Part 4 measured the equivalent
assertion in `RExceptions` on a then-current binary and reports
`AssertionError: ArrayStoreException text moved during warm-up at i=500:
cold=[java.lang.Integer] hot=[no-throw]`.

CratonVM, after: **PREDICTED** `PASS RArrayStoreTiers (63 checks)` on both runs,
and `RExceptions` predicted to go green at its `i=500` tier-parity assertion.

## 6. Nomination — restore the fast path

Not part of this fix. Add a check-only helper so the inline lowering can come
back:

* `vm/src/jit/helpers.rs` — `jit_aastore_check(vm_ptr, array_ptr, val)`,
  containing only the `val != 0` / `element_type_of` / `aastore_element_assignable`
  block currently inside `jit_aastore`, setting the pending exception and
  returning.
* `jit-api/src/helpers_abi.rs` and `jit-api/src/lib.rs` — the matching ABI slot.
* `jit/src/x64/bytecode_walk.rs` `0x53` — restore the inline lowering with a
  call to the new helper between the bounds check and the SATB barrier.

Land it only with `RArrayStoreTiers` green **both** ways, and only after
measuring what the call actually costs — R20's inline store was a real win and
this record does not have the number that would justify re-spending it.
