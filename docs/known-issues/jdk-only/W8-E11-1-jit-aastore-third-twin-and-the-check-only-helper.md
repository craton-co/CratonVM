# W8-E11-1 — the third `aastore` twin, and the check-only helper that lets the inline store come back (APPLIED + NOMINATION SET)

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

> **STATUS: applied in the two files this lane owns** — `vm/src/jit/helpers.rs`
> and `vm/src/runtime/interpreter/lambda.rs`. **Every CratonVM "after" below is
> PREDICTED**: this lane may not build or run the binary. The HotSpot columns
> are MEASURED in this session, on this host, and the transcripts are in §1.
> §6 is an ATOMIC NOMINATION SET across three files this lane does not own —
> none of the three is landable alone.

Lane E11, 2026-08-13. Oracle: HotSpot `openjdk 25.0.3 2026-04-21 LTS`
(`25.0.3+9-LTS`, Microsoft-13877124), windows/x64.
Probes: `scratchpad/e11/AseName.java`, `scratchpad/e11/FastThrow.java`,
`scratchpad/e11/AastoreCost.java` — written and run this session, held in the
session scratchpad (`…\ee7c36dd-7994-4891-bb11-64e5eb0eadd5\scratchpad\e11\`,
compiled into `out/` beside them) because the repo has no `scratchpad/` tree.
All three are self-contained single-file programs; `AastoreCost` takes
`-Dreps=N`.

Predecessors: `W7-37` §"Part 4" (diagnosed), `W7-38` (wired the emitter to the
helper), `W8-E6-1` (fixed the two interpreter message builders, and nominated
this one in its §7).

---

## 1. The oracle, measured

### 1.1 Every message form asserted here

`java -cp out AseName` — each store executes **exactly once**, so none of it is
in HotSpot's fast-throw regime (§1.2). Columns are
`tag | exception | getMessage() | value.getClass().getName()`.

```text
P1_StrArr_lt_Integer|java.lang.ArrayStoreException|java.lang.Integer|java.lang.Integer
P2_StrArr_lt_Nested|java.lang.ArrayStoreException|AseName$Inner|AseName$Inner
P3_StrArr_lt_DefaultPkg|java.lang.ArrayStoreException|Plain|Plain
A1_StrArr2d_lt_IntegerArr|java.lang.ArrayStoreException|[Ljava.lang.Integer;|[Ljava.lang.Integer;
A2_StrArr2d_lt_StrArr2d|java.lang.ArrayStoreException|[[Ljava.lang.String;|[[Ljava.lang.String;
A3_StrArr_lt_DefaultPkgArr|java.lang.ArrayStoreException|[LPlain;|[LPlain;
B1_StrArr2d_lt_intArr|java.lang.ArrayStoreException|[I|[I
B2_StrArr2d_lt_int2dArr|java.lang.ArrayStoreException|[[I|[[I
B3_StrArr2d_lt_byteArr|java.lang.ArrayStoreException|[B|[B
N1_ObjArr2d_lt_ObjArr|NO-THROW|-|[Ljava.lang.Object;
N2_StrArr2d_lt_StrArr|NO-THROW|-|[Ljava.lang.String;
N3_StrArr_lt_null|NO-THROW|-|(null)
```

`java -Xint -cp out AseName` is **byte-identical**. The message is not a
tier-dependent property in the cold regime.

Three rules, all of which the fix has to satisfy at once:

1. An **array value** is named in JVMS descriptor form with dots
   (`[Ljava.lang.Integer;`, `[[I`, `[B`) — never the component, never JEP 358
   source form. Rows A1/A2/A3/B1/B2/B3.
2. A **plain class** is binary-with-dots, `$` preserved for nested
   (`AseName$Inner`), unqualified for the default package (`Plain`). P1/P2/P3.
3. The message names the **value**, never the array or its component.

The fourth column is the independent cross-check: `getMessage()` equals
`value.getClass().getName()` on every throwing row, which is
`Klass::external_name()` by another door.

Rows N1–N3 are negative controls. A check that throws on everything, or one
that mishandles a null element, passes a fixture built only from throwing
shapes.

### 1.2 Hot-tier messages ARE assertable — `W8-E6-1` §9 can be narrowed

`W8-E6-1` §9 recorded hot-tier ASE messages as "unmeasured and unmeasurable
from this fixture". The first half is right; the second is only true of the
oracle **as configured by default**.

`java -cp out FastThrow` (30 000 iterations of one throwing shape):

```text
first=[[Ljava.lang.Integer;] last=[null] messageMovedAt=5175 stackTraceEmptyAt=5175
```

`java -XX:-OmitStackTraceInFastThrow -cp out FastThrow`:

```text
first=[[Ljava.lang.Integer;] last=[[Ljava.lang.Integer;] messageMovedAt=-1 stackTraceEmptyAt=-1
```

So:

* HotSpot's *default* hot answer is `null`, at i=5175 on this run, together
  with an empty stack trace — `-XX:+OmitStackTraceInFastThrow` swapping in the
  preallocated instance. A raw differential of hot `getMessage()` against a
  default-configured HotSpot would demand CratonVM *lose* the message, which is
  the oracle's optimizer, not the JVMS.
* With the flag off, the correct hot value is **`[Ljava.lang.Integer;`** for
  all 30 000 iterations. That is the oracle for a hot-tier assertion, and it is
  obtainable — the assertion just has to be *CratonVM's* tier-invariance
  (`hot == cold == HotSpot's cold`), with `-XX:-OmitStackTraceInFastThrow` on
  the oracle side when the oracle is consulted hot at all.

This is the same trap as `W7-40`'s: asserting a message across tiers measures
the oracle's optimizer unless the optimizer is turned off first.

## 2. The defect: the third twin, and it is NOT latent

`jit_aastore` built its `ArrayStoreException` message with

```rust
.get_class(vm.mem.heap.class_id_of(value_ref))
.map(|c| c.name.to_string())
```

— the raw lookup `W8-E6-1` removed from both interpreter arms. On a reference
array the header class id holds the **component's** class **by design**
(`typecheck::array_descriptor_of`: *"The class_id on a Reference array holds the
component class id"*; and at length on `cce_display_class_name`, where the same
trap once produced `java.lang.String cannot be cast to java.lang.String` and was
chased for a session as a class-identity split). So for an array-valued element
the message was off by **exactly one array dimension**, and correct for
everything else.

### Reachability — established, not assumed

The task brief expected this to be latent. **It is not.** `W7-38`'s emitter
change has landed: `jit/src/x64/bytecode_walk.rs`'s `0x53` arm now reads

```rust
self.emit_call_absolute(self.helpers.aastore);
self.emit_post_invoke_exception_check(b'V');
```

`0x53` has exactly one emission site (the other tiers refuse the opcode:
`ir_lower.rs` `latch_bailout`s `MemKind::Ref` stores, `aarch64_backend.rs` lists
`0x53` in `object_model_opcodes_are_all_unsupported`, `escape_analysis.rs`
emits nothing). So compiled code reaches this message builder on every illegal
reference array store, and an application that catches `ArrayStoreException` in
a hot method and reads `getMessage()` sees the wrong name today.

What **is** latent is the *fixture's* view of it. `RArrayStoreTiers` asserts
messages in pass 1 only — one execution per shape, before pass 2's 3000-iteration
tier-parity loop — and pass 2 compares only the exception **kind**. That is a
deliberate and correct design (§1.2 is why), and its consequence is that
`RArrayStoreTiers` would have gone green with this twin still wrong. **A green
suite is not what cleared the JIT here; reading the emitter is.**

Leaving it would also have re-created the exact structure `W7-38` documents:
one JVMS rule, two implementations, each file's comment asserting something
about the other that the build cannot check.

## 3. What was applied

### 3.1 `vm/src/runtime/interpreter/lambda.rs` — widen the helper

`pub(super)` → `pub(crate)`, with a doc paragraph saying why. `interpreter.rs`
already does `pub use lambda::*;` at line 7847 and a glob re-export caps at the
item's own visibility, so this is the entire export change — no new re-export.
The JIT reaches it as `crate::runtime::interpreter::cce_display_class_name`, the
same module path `helpers.rs` already uses for `aastore_element_assignable`.

`cce_display_class_name` now has **five** callers for the five sites that ask
the question: `lambda.rs`'s SAM argument check, `opcodes.rs`'s `checkcast`, both
interpreter `aastore` arms, and the JIT. **No new formatter was written** — this
session has now found nine separate instances of "the correct helper exists and
the callers don't use it", and a fourth spelling of this parse is how the tenth
gets made.

### 3.2 `vm/src/jit/helpers.rs` — extract `jit_aastore_check`, and fix the message in it

The two tasks land as one edit on purpose. The check block was **moved** out of
`jit_aastore` into a new `jit_aastore_check`, the message builder was fixed
**there**, and `jit_aastore` now calls it:

```rust
if jit_aastore_check(vm_ptr, array_ptr, val) == i64::MIN {
    return;
}
```

so the crate contains exactly **one** ArrayStoreException check and **one** ASE
message builder, before and after §6 lands. Copying the block into a second
helper — leaving `jit_aastore`'s own copy in place for a caller list that is
about to become empty again — would have rebuilt the original defect's
structure while fixing its instance.

Inside the new helper, the raw lookup became the two-statement reuse
(`cce_display_class_name` takes the class-manager read lock itself, so folding
it into the lookup expression would hold that guard across the call):

```rust
let raw_elem_name = /* unchanged lookup */;
let elem_cls = crate::runtime::interpreter::cce_display_class_name(
    vm,
    value_ref,
    &raw_elem_name,
);
```

Why the one-liner is correct rather than merely shorter:

* **It returns the INTERNAL (slashed) name, and that is what this site wants.**
  `throw_runtime_error`'s funnel (`exceptions::hotspot_vm_type_error_message`)
  dots it. `jit_aastore` already went through that funnel, which is why the
  pre-fix message read `java.lang.Integer` (dotted) rather than
  `java/lang/Integer` — the dotting was never the bug.
* **The funnel leaves primitive descriptors and default-package names alone**,
  because its `rewritable` predicate is `contains('/') && !contains(whitespace)`.
  Rows B1/B2/B3 (`[I`, `[[I`, `[B`) and A3 (`[LPlain;`) therefore come out right
  with no special case — their internal and external forms are equal.
* **The `"?"` fallback strictly improves.** `cce_display_class_name` tests the
  heap header first, so an array whose component class is unresolvable now
  yields a descriptor instead of `"?"`.

Two stale comments were corrected in the same edit, because they are the
mechanism this record is about. `jit_aastore` carried *"on x64 this arm does not
run at all, because nothing calls this function"* — true when written, falsified
by `W7-38`, exactly mirroring the emitter comment (*"the current `jit_aastore`
helper does NOT enforce the ASE check"*) that `W7-38` had to correct in the
other direction. Both halves now describe the tree as it is, and each says to
fix the other in the same commit if the call is ever removed again.

### 3.3 The new helper's contract

`jit_aastore_check(vm_ptr, array_ptr, val) -> i64`. `i64::MIN` = **refused**, an
`ArrayStoreException` is pending on this thread, the caller must skip the store,
the SATB pre-write barrier and the card mark. `0` = proceed.

Two deliberate choices:

* **A defined `i64` sentinel, not `-> ()`.** `emit_post_invoke_exception_check`
  works by `CMP RAX, i64::MIN; JE bail`. After a `-> ()` extern "C" call RAX is
  *undefined*, so the check the current `0x53` arm emits is testing garbage and
  the exception in fact surfaces only via the interpreter's post-JIT-return
  drain. Returning the sentinel makes that guard mean what it looks like it
  means. (The existing `putstatic_*` helpers already use exactly this
  protocol.)
* **Fails open in three places, each on purpose**: a null `val` (JVMS §6.5 —
  always storable), an implausible `array_ptr` (the caller's null check has
  already run, so a stale word here would SIGSEGV *inside Rust*, strictly harder
  to diagnose than the store faulting at its own site), and a failure to
  construct the throwable (fall through and store, the pre-`W7-38` degradation).
  A screen that answered `i64::MIN` on "don't know" would drop a legal store
  **and** fabricate an exception — worse than the defect it guards.

Ordering is unchanged and load-bearing: the check runs after the null and bounds
checks and before the barrier and store. That is JVMS §6.5 precedence
(NPE → AIOOBE → ASE), and it is not theoretical — `RArrayStoreTiers` s15 caught
the interpreter fast path reporting ASE for a past-the-end index.

`note_jit_boundary()` is called unconditionally at the top of the new helper,
like every sibling: the refusal path allocates a throwable. `jit_aastore` bumps
it too, so the delegated path bumps twice — a redundant bump only forces a
re-scan and is never unsound in the other direction. A helper that bumped it
only on the allocating path would be a soundness argument nobody re-derives
correctly later.

### 3.4 Unit test added

`jit_aastore_check_fails_open_and_never_derefs_vm_on_the_screens` pins all three
fail-open screens, two of them with a **null `vm_ptr`** — which is itself the
assertion that those screens return before constructing the `&SharedVm`. The
third uses the existing `alloc_test_array` to build a real reference array whose
component class is unresolvable, and asserts `0`: no false ASE. It does not
attempt the throwing path, which needs a loaded class graph and is the
fixture's job.

## 4. Verification (PREDICTED — nothing was built or run here)

```
regression-suite: RArrayStoreTiers, BOTH ways
  cratonvm.exe --java-home $JAVA_HOME --jdk-only         -cp out RArrayStoreTiers
  cratonvm.exe --java-home $JAVA_HOME --jdk-only --nojit -cp out RArrayStoreTiers
```

| assertion | predicted | why it is the falsifying one |
|---|---|---|
| overall | `PASS RArrayStoreTiers (63 checks)`, both runs | the orchestrator measured 4 → 4 after `W7-38`; `W8-E6-1` predicts those 4 → 0 |
| s01/s02/s03/s05 COLD-MESSAGE | unchanged | regression guard: `cce_display_class_name` must be a **no-op** for a plain class. A change here means its `UnmodifiableMap` arm is over-firing |
| s06–s13 kind = `no-throw`, both runs | unchanged | the extraction must not create a false ASE. If any of these turns red, the *move* is wrong, not the message fix — the message is built only after `aastore_element_assignable` has already refused |
| s14 `NullPointerException`, s15 `AIOOBE` | unchanged | precedence preserved across the extraction |
| **`RArrayStoreTiers` alone cannot falsify §3.2** | — | it asserts messages cold-only. See below |

**The suite does not cover this fix.** The falsifying run is `scratchpad/e11/AseName.java`
under both VMs, diffed against §1.1 — it carries the six array-valued rows the
fixture has one of, plus the primitive-descriptor rows that exercise the funnel's
no-`/` arm, plus three negative controls. Then, for the tier claim specifically:

```
java -XX:-OmitStackTraceInFastThrow -cp out FastThrow    # oracle: [Ljava.lang.Integer; throughout
cratonvm.exe --jdk-only         -cp out FastThrow        # predicted: same, all 30000
cratonvm.exe --jdk-only --nojit -cp out FastThrow        # predicted: same, all 30000
```

Predicted `first=[[Ljava.lang.Integer;] last=[[Ljava.lang.Integer;] messageMovedAt=-1`
on CratonVM in **both** runs. `messageMovedAt >= 0` on the JIT run and `-1` on
`--nojit` localises a residual to the compiled tier. Before the fix the JIT run
would read `first=[java.lang.Integer]` — which is also the check that §2's
reachability claim is right: if the pre-fix JIT run printed the *correct*
message, the emitter is not on this path and §2 is wrong.

## 5. What this does NOT establish

* **No throughput claim.** §6 exists precisely because the number is missing.
* **The `-> ()`/undefined-RAX observation in §3.3 is not fixed here.** The
  `0x53` arm's `emit_post_invoke_exception_check(b'V')` after a void helper is
  in another lane's file. It is not a correctness hole — the interpreter's
  post-JIT drain surfaces the exception on return — but the guard does not do
  what its name says, and the exception is delivered later than the emitter
  appears to intend. §6's emitter change makes it correct as a side effect,
  because the helper it calls returns a real sentinel.
* **`array_descriptor_of` still fails soft to `[Ljava/lang/Object;`** when the
  component class cannot be resolved. Pre-existing, shared with `checkcast`,
  and now shared with the JIT: it cannot affect any measured row (every
  component in §1.1 is loaded) but it would produce a wrong-but-plausible
  message for an array of an unresolvable component.

## 6. NOMINATION — ATOMIC SET: restore the inline store (`W7-38` §6)

> **These four edits MUST land in ONE commit.** A helper with no ABI slot is
> dead code; an ABI slot with no initializer fails `validate()`; an emitter call
> against a slot that does not exist **does not compile**. The `jit_aastore_check`
> half is already applied (§3.2) and is inert until this set lands.
>
> **AND: do not land any of it until the two conditions in §6.5 are met.**

Files, none of which this lane owns except (d), which is held back deliberately
because it cannot compile before (a):

### (a) `jit-api/src/lib.rs` — the struct field, appended at the END

The field must be **appended**; the ABI is append-only and `GOLDEN_HELPER_OFFSETS`
is a literal table keyed by name. `ldc_class_cp` at 496 is currently last, so the
new slot is 504 and no existing offset moves.

old:
```rust
    pub ldc_class_cp: usize,
}
```
new:
```rust
    pub ldc_class_cp: usize,
    /// JVMS §6.5 *aastore* covariance check ONLY — `extern "C" fn(vm_ptr: i64,
    /// array_ptr: i64, val: i64) -> i64`. Returns `i64::MIN` when the store
    /// must be refused and an `ArrayStoreException` has been published, `0`
    /// when it may proceed.
    ///
    /// Exists so the `0x53` lowering can keep the inline
    /// `MOV [array + index*8 + HEADER_SIZE], val` and call out only for the
    /// type check, instead of routing the whole opcode through
    /// [`Self::aastore`] (W7-38 restored correctness that way and paid one
    /// call per reference array store for it).
    ///
    /// `0` = not wired (hand-built test tables) → the backend must fall back
    /// to calling [`Self::aastore`], which is the complete opcode. It must NOT
    /// fall back to the bare inline store: that is the heap-type-confusion
    /// defect W7-38 fixed. Appended at the END of the struct so all prior
    /// golden offsets stay stable.
    pub aastore_check: usize,
}
```

old:
```rust
    // Optional: 0 makes the single-pass backend refuse an `ldc <Class>` site
    // and bail the compile — the pre-fix behaviour.
    (ldc_class_cp,                   FieldKind::OptionalPtr),
}
```
new:
```rust
    // Optional: 0 makes the single-pass backend refuse an `ldc <Class>` site
    // and bail the compile — the pre-fix behaviour.
    (ldc_class_cp,                   FieldKind::OptionalPtr),
    // Optional: 0 makes the `0x53` lowering call `aastore` (the complete
    // opcode) instead of inline-store-plus-check. Never the bare inline store.
    (aastore_check,                  FieldKind::OptionalPtr),
}
```

old:
```rust
const _: () = assert!(
    JitRuntimeHelpers::NUM_FIELDS == 63,
```
new:
```rust
const _: () = assert!(
    JitRuntimeHelpers::NUM_FIELDS == 64,
```

and in `mod tests`, append the probe row and bump the two literals:

old:
```rust
            (
                62,
                "ldc_class_cp",
                std::mem::offset_of!(JitRuntimeHelpers, ldc_class_cp),
            ),
        ];
```
new:
```rust
            (
                62,
                "ldc_class_cp",
                std::mem::offset_of!(JitRuntimeHelpers, ldc_class_cp),
            ),
            (
                63,
                "aastore_check",
                std::mem::offset_of!(JitRuntimeHelpers, aastore_check),
            ),
        ];
```

old:
```rust
        assert_eq!(JitRuntimeHelpers::NUM_FIELDS, 63);
```
new:
```rust
        assert_eq!(JitRuntimeHelpers::NUM_FIELDS, 64);
```

The two hand-built test tables in that module (`lib.rs` ~1716
`ldc_class_cp: 0x11B8` and ~1951 `ldc_class_cp: 0`) each need one added line —
`aastore_check: 0x11C0` and `aastore_check: 0` respectively. Mechanical, and the
compiler names both; they are listed so nobody reads a struct-literal error as a
sign the append was wrong.

`zero_field_by_name` (~2651) needs **no** arm: it covers only `RequiredPtr`
fields, and `aastore_check` is optional. Adding one would be harmless but would
also be the first hint that someone marked the slot `required` — which it must
not be, because `0` has a correct meaning here (route to `aastore`).

### (b) `jit-api/src/helpers_abi.rs` — the typed slot, the descriptor row, the golden offset, the ledger

old:
```rust
    HelperFnLdcClassCp, ldc_class_cp, ldc_class_cp_fn, (i64, i64, i64) -> i64;
}
```
new:
```rust
    HelperFnLdcClassCp, ldc_class_cp, ldc_class_cp_fn, (i64, i64, i64) -> i64;
    // JVMS §6.5 aastore covariance check only — (vm_ptr, array_ptr, val) ->
    // `i64::MIN` = refused (ArrayStoreException published) / `0` = proceed.
    // NOT the store: the caller keeps the inline MOV, the SATB pre-write
    // barrier and the card mark.
    HelperFnAastoreCheck, aastore_check, aastore_check_fn, (i64, i64, i64) -> i64;
}
```

old:
```rust
    // Optional: 0 makes the single-pass backend refuse an `ldc <Class>` site.
    (ldc_class_cp,                   Function, false),
}
```
new:
```rust
    // Optional: 0 makes the single-pass backend refuse an `ldc <Class>` site.
    (ldc_class_cp,                   Function, false),
    // Optional: 0 makes the `0x53` lowering route the whole opcode to
    // `aastore` instead. Not `required`: the fallback is correct, just slower.
    (aastore_check,                  Function, false),
}
```

old:
```rust
    ("ldc_class_cp", 496),
];
```
new:
```rust
    ("ldc_class_cp", 496),
    ("aastore_check", 504),
];
```

old:
```rust
    // v4 — appended `ldc_class_cp`, so that an `ldc <Class>` compiles at all.
    HelperAbiRevision {
        version: 4,
        num_fields: 63,
        size: 504,
    },
];
```
new:
```rust
    // v4 — appended `ldc_class_cp`, so that an `ldc <Class>` compiles at all.
    HelperAbiRevision {
        version: 4,
        num_fields: 63,
        size: 504,
    },
    // v5 — appended `aastore_check`, so the `0x53` lowering can keep its
    // inline store and call out only for the JVMS §6.5 covariance check.
    HelperAbiRevision {
        version: 5,
        num_fields: 64,
        size: 512,
    },
];
```

old:
```rust
pub const JIT_HELPERS_ABI_VERSION: u32 = 4;
```
new:
```rust
pub const JIT_HELPERS_ABI_VERSION: u32 = 5;
```

(The doc comment above that constant says *"`4` is the revision of the 63-field,
504-byte table shipped today"* — update it to `5` / 64 / 512 in the same edit.
The `ABI_REVISIONS` const assertion will refuse the build otherwise, which is
the whole point of that ledger: the `monitor_enter`/`monitor_exit` append that
forgot the bump is what it was built to make unrepresentable.)

### (c) `jit/src/x64/bytecode_walk.rs` — the `0x53` arm

Restore the inline lowering and call the check between the bounds check and the
SATB barrier. **The ordering is not stylistic**: JVMS §6.5 is NPE → AIOOBE →
ASE, and putting the check before the bounds check reproduces the exact defect
`RArrayStoreTiers` s15 caught in the interpreter fast path.

The pre-`W7-38` inline sequence is still in the tree —
`emit_ref_astore_regs` (`jit/src/x64/arrays.rs`) was left with no callers
deliberately, for this. Sketch, to be reconciled with that function's actual
register contract:

```rust
0x53 => {
    self.flush_scratch_registers();
    let val_slot = self.pop_stack();
    let index_slot = self.pop_stack();
    let array_slot = self.pop_stack();
    if self.helpers.aastore_check == 0 {
        // Not wired: route the WHOLE opcode to the complete helper. Never
        // fall back to the bare inline store — that is the tier-dependent
        // heap type confusion W7-38 fixed.
        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
        self.load_slot_to_reg(ARG_REGS[1], array_slot);
        self.load_slot_to_reg(ARG_REGS[2], index_slot);
        self.load_slot_to_reg(ARG_REGS[3], val_slot);
        self.emit_call_absolute(self.helpers.aastore);
        self.emit_post_invoke_exception_check(b'V');
        pc += 1;
        // (structure the arm so this path returns/skips the block below)
    } else {
        self.load_slot_to_reg(RAX, array_slot);
        self.load_slot_to_reg(RCX, index_slot);
        self.emit_null_check_array_store_at(code, pc);   // NPE first
        self.emit_bounds_check(pc);                      // then AIOOBE
        // jit_aastore_check(vm_ptr, array_ptr, val) -> i64::MIN | 0
        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
        self.load_slot_to_reg(ARG_REGS[1], array_slot);
        self.load_slot_to_reg(ARG_REGS[2], val_slot);
        self.emit_call_absolute(self.helpers.aastore_check);
        self.emit_post_invoke_exception_check(b'V');     // CMP RAX, i64::MIN
        // then, and only then, the SATB pre-write barrier + store + card mark
        self.load_slot_to_reg(RAX, array_slot);
        self.load_slot_to_reg(RCX, index_slot);
        self.load_slot_to_reg(RDX, val_slot);
        self.emit_ref_astore_regs();
        pc += 1;
    }
}
```

Four things whoever writes this must check rather than assume, because each is
a way to make it silently wrong:

1. **`flush_scratch_registers` before the three `load_slot_to_reg`s.** It
   rewrites every register-resident stack slot to a frame slot, so the operand
   loads all read from memory and cannot clobber each other's source register —
   on either ABI (`ARG_REGS` = RCX/RDX/R8/R9 on Windows, RDI/RSI/RDX/RCX on
   SysV). Both the current arm and the pre-`W7-38` arm relied on this.
2. **The check call clobbers the caller-saved registers**, so `array_slot` /
   `index_slot` / `val_slot` must be **re-loaded** afterwards for the store. The
   sketch does; a version that hoists the loads above the call is wrong.
3. **`0x53` is a one-byte opcode**, so `emit_post_invoke_exception_check` keeps
   *this* pc as the throw pc — required by the handler `[start_pc, end_pc)`
   range test.
4. **`RAX` is now a defined value** (§3.3), so this `emit_post_invoke_exception_check`
   actually guards; on the `aastore` fallback path it still does not, and that
   path relies on the post-JIT-return drain as it does today.

### (d) `vm/src/jit/helpers.rs` — the initializer and the type pin (this lane's file, HELD BACK)

Deliberately **not applied**: both lines reference a struct field that does not
exist yet, so applying them now breaks the `vm` crate's build. They belong in
the same commit as (a) and (b).

old:
```rust
        aastore: jit_aastore as *const () as usize,
```
new:
```rust
        aastore: jit_aastore as *const () as usize,
        // Check-only companion; see `jit_aastore_check`. Always wired in
        // production, so the `0x53` lowering keeps its inline store.
        aastore_check: jit_aastore_check as *const () as usize,
```

old:
```rust
    let _: HelperFnAastore = jit_aastore;
```
new:
```rust
    let _: HelperFnAastore = jit_aastore;
    let _: HelperFnAastoreCheck = jit_aastore_check;
```

The second line is the compile-time link that this whole record exists because
its absence permitted: it makes a signature change to `jit_aastore_check`
without a matching `helpers_abi.rs` row a **build error** rather than a comment
that quietly stops being true.

### 6.5 The two conditions — both are gates, not advice

**Condition 1 — `RArrayStoreTiers` green BOTH with and without `--nojit`.**
A single green run cannot distinguish "both tiers are right" from "the JIT never
engaged". Red without `--nojit` and green with it localises a residual to the
compiled tier; red both ways means the shared check is wrong. This is
`W7-38` §5's own condition and it is restated here because §6 re-opens exactly
the code path it was written for. Add the `FastThrow` pair from §4 to it: with
the inline store restored, the message builder is reached from a *different*
lowering, and cold-only message assertions cannot see that.

**Condition 2 — measure the call first.** `W7-38` §6 says, in its own words,
that R20's inline store "was a real win and this record does not have the number
that would justify re-spending it". Re-spending a *measured* optimisation on an
*unmeasured* guess is not an improvement, and the guess is not obviously right:
the check-only helper still costs a `CALL` per reference array store, plus
`element_type_of`, plus `aastore_element_assignable`, plus a class-manager read
lock. Against `jit_aastore` it saves the null check, the bounds check, the
barrier dispatch and the store — real, but a fraction of the whole. It is
entirely possible that the honest measurement says the fast path is not worth
the four-file ABI change, and that is a legitimate outcome of running it.

**The benchmark that produces the number** is `scratchpad/e11/AastoreCost.java`
(written this session, HotSpot-verified as a harness; **its numbers on HotSpot
are not the answer** — HotSpot vectorises the `intStore` control to ~0.2 ns/store,
which CratonVM will not). Three kernels, each in its **own method** so each gets
its own compiled site:

* `refStore` — `Object[]` that is really a `String[]`, storing `String`s. The
  stores are **legal**, which is the path that runs billions of times: call +
  `element_type_of` + `aastore_element_assignable` returning true + return.
* `objStore` — a genuine `Object[]`. Same call, but the assignability check
  short-circuits on the `Object[]`-accepts-everything rule before any hierarchy
  walk. `refStore − objStore` is the cost of the walk.
* `intStore` — `int[]`, opcode `0x4f`, inline on every build. The control that
  removes loop / bounds / JIT-entry overhead, so the figure is quotable as a
  difference rather than an absolute that silently includes the loop.

Protocol, all of it load-bearing on a shared host:

1. Three builds of the **same commit**, differing only in the `0x53` arm:
   **A** = today (whole opcode → `jit_aastore`), **B** = §6 (inline store +
   `jit_aastore_check`), **C** = bare inline store, no check — *not landable*,
   it is the R20 floor, and without it "B is faster than A" has no scale.
2. **ABBA-interleaved**, never A-then-B, and verify the arms differ by binary
   hash before believing any gap.
3. Quote **`(refStore − intStore) / stores` in ns/store, and the A:B:C ratio** —
   absolute wall time on a shared host is worthless.
4. The printed checksum must be identical on every arm and every rep. A build
   whose checksum differs did not do the same work.
5. Get `jit_entries` (`CRATONVM_DBG_JIT_SCAN_PROF=1`) before theorising about
   the result: 300–700 ns/entry means the kernel is call-density-bound and the
   aastore delta is being read out of the wrong signal.
6. Then, and separately, `regression-suite/perf/run-cratonbench-gate.sh -Exe <abs path>`
   against `cratonbench-baseline-azure-epyc.tsv`, bracketed by its
   `reliability-gate.sh` PREFLIGHT/POSTFLIGHT. That answers a **different**
   question — "did this move anything else" — and cannot answer the first:
   reference array stores are a few percent of a mixed workload, so the gate
   would report even a tripled per-store cost as noise. The two are not
   substitutes and both are required.

## 6.6 NOMINATION — the index row (`docs/known-issues/jdk-only/INDEX.md`, not a new file)

In the `## JIT / typecheck` table, after the `W7-38` row.

old:
```
| W7-38-jit-aastore-never-called-its-own-check | the JIT lowered `aastore` inline, bypassing its check | FIXED-UNVERIFIED | MIXED | after values explicitly PREDICTED |
```
new:
```
| W7-38-jit-aastore-never-called-its-own-check | the JIT lowered `aastore` inline, bypassing its check | FIXED-UNVERIFIED | MIXED | after values explicitly PREDICTED |
| W8-E11-1-jit-aastore-third-twin-and-the-check-only-helper | the JIT's ASE message named the component, not the array | FIXED-UNVERIFIED | MIXED | REACHABLE, not latent — the fixture asserts messages cold-only. Carries an ATOMIC 4-file NOMINATION SET (`W7-38` §6) gated on two measured conditions |
```

## 7. Residual

* **The fixture cannot see message fixes in the hot tier**, by design. Until a
  hot-message vector exists (§4's `FastThrow` pair is the smallest one), a green
  `RArrayStoreTiers` says nothing about what compiled code prints. That is not a
  criticism of the fixture — §1.2 is why it is built that way — it is a gap that
  has to be filled by a second instrument, not by trusting the first.
* **`emit_post_invoke_exception_check` after a `-> ()` helper tests undefined
  RAX** (§3.3, §5). Not this lane's file. It is inert today and becomes correct
  under §6; if §6 is declined, it is worth a one-line fix on its own.
* **`W8-E6-1` §9's second clause is now narrowed** by §1.2: hot ASE messages are
  measurable, with `-XX:-OmitStackTraceInFastThrow` on the oracle.
