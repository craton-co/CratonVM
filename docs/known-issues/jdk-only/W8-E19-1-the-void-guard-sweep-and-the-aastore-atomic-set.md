# W8-E19-1 — the undefined-RAX guard sweep, the `aastore` ATOMIC SET (2 of 5 applied), and a second not-single-site opcode

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

> **STATUS: two of FIVE files applied; THE TREE DOES NOT BUILD UNTIL §3's
> NOMINATION LANDS IN THE SAME COMMIT.** This lane owns
> `jit/src/x64/bytecode_walk.rs` and `vm/src/jit/helpers.rs` and has written
> both halves of the atomic set into them. The other three are §3 and are NOT
> applied — this lane owns none of them. (`W8-E11-1` §6 called this a four-file
> set; §3(c) is a fifth it did not have.) Applying two of five is the
> documented, intended state of a handoff, not an oversight:
> `helpers.aastore_check` does not exist as a struct field yet, so
> **`cargo build -p cratonvm-jit` and `-p cratonvm-vm` both fail today.**
>
> **Every CratonVM "after" in this record is PREDICTED.** This lane may not run
> `cargo` and may not execute the binary. No number here was measured on
> CratonVM.

Lane E19, 2026-08-13. Predecessors: `W7-38` (wired the emitter to the whole
helper), `W8-E11-1` (extracted `jit_aastore_check`, fixed the third ASE-message
twin, and wrote the nomination this record completes and extends).

---

## 1. TASK 1 — the void-guard sweep, with a verdict per site

### 1.1 What was swept, and how the question was decided

`emit_post_invoke_exception_check` (`jit/src/x64/deopt_stubs.rs:990`) is
`MOV R10, i64::MIN ; CMP RAX, R10 ; JE bail` (plus, for `J`/`D`/`F`, the cold
`jit_dispatch_threw` peek). It can only work if RAX holds a **defined** value at
that point. `W8-E11-1` §3.3 observed that the `0x53` arm emitted it after
`jit_aastore`, which is declared `-> ()`, and did not sweep for siblings.

The decidable question is: **does the immediately preceding `CALL` target a
helper whose Rust signature returns `()`?** That is not a judgement call —
`jit-api/src/helpers_abi.rs`'s `helper_fn_slots!` block is the declared answer,
and its own test (`helpers_abi.rs:1758`) enumerates the `-> ()` slots:

```
bastore, iastore, aastore,
putfield_int, putfield_long, putfield_float, putfield_double, putfield_object,
write_barrier, satb_pre_write_barrier,
frame_record, jit_npe_with_action, safepoint_slow_path, set_throw_bci
```

**The `ret_type` argument is not the answer to this question and reads as if it
were.** `b'V'` is the *Java opcode's* return descriptor; it selects the guard's
*shape* (plain `CMP` vs. the wide-value peek). Four sites pass `b'V'` after a
helper that returns a perfectly good `i64`. Conflating the two is the single
easiest way to mis-sweep this, and is why the table below has a separate column
for each.

### 1.2 The census: 19 sites before this lane's edit, 1 hole

Line numbers are POST-edit (§2.1 added ~90 lines to `bytecode_walk.rs`
above every one of them); the pre-edit number is in parentheses where it
differs, because the earlier records cite those.

| # | site | preceding `CALL` target | helper's Rust return | `ret_type` passed | RAX defined? | verdict |
|---|---|---|---|---|---|---|
| 1 | `bytecode_walk.rs:1883` (was 1843) | `helpers.aastore` | **`-> ()`** | `b'V'` | **NO** | **the hole** — see §1.3 |
| — | `bytecode_walk.rs:1903` | `helpers.aastore_check` | `-> i64` | `b'V'` | yes | **NEW** in §2.1; the inline arm |
| 2 | `bytecode_walk.rs:3948` (was 3856) | `getstatic` | `-> i64` | `type_tag` | yes | sound |
| 3 | `bytecode_walk.rs:4027` (was 3935) | `putstatic_{int,long,float,double,object}` | `-> i64` | `b'V'` | yes | sound; `b'V'` is the opcode, the helper still returns the sentinel |
| 4 | `bytecode_walk.rs:4245` (was 4153) | `getfield` (compact guarded slow path) | `-> i64` | `type_tag` | yes | sound |
| 5 | `bytecode_walk.rs:4390` (was 4298) | `getfield` (legacy guarded slow path) | `-> i64` | `type_tag` | yes | sound |
| 6 | `bytecode_walk.rs:4426` (was 4334) | `getfield` (resolved, inline disabled) | `-> i64` | `type_tag` | yes | sound |
| 7 | `bytecode_walk.rs:4446` (was 4354) | `getfield` (unresolved) | `-> i64` | `b'J'` | yes | sound; `b'J'` is deliberately over-strict |
| 8 | `bytecode_walk.rs:4827` (was 4735) | `lambda_int_to_double` | `-> i64` | `b'D'` | yes | sound |
| 9 | `bytecode_walk.rs:5979` (was 5887) | `invoke_dispatch` (arraycopy) | `-> i64` | `b'V'` | yes | sound; `b'V'` = the *callee's* descriptor |
| 10 | `bytecode_walk.rs:6690` (was 6598) | compiled callee entry (direct `invokestatic`) | JIT return ABI | `ret_type` | yes — see §1.4 | sound |
| 11 | `bytecode_walk.rs:6824` (was 6732) | `invoke_dispatch` | `-> i64` | `info_ref.return_type` | yes | sound |
| 12 | `bytecode_walk.rs:7082` (was 6990) | self-recursive direct call | JIT return ABI | `b'I'` | yes | sound (documented `J`/`D` collision, unrelated) |
| 13 | `bytecode_walk.rs:8718` (was 8626) | compiled callee entry (direct virtual/special) | JIT return ABI | `ret_type` | yes | sound |
| 14 | `bytecode_walk.rs:9878` (was 9786) | `invoke_dispatch` / `invoke_virtual_mic` | `-> i64` | `info_ref.return_type` | yes | sound |
| 15 | `bytecode_walk.rs:10575` (was 10483) | `checkcast` | `-> i64` | `b'L'` | yes | sound |
| 16 | `bytecode_walk.rs:10813` (was 10721) | `monitor_enter` / `monitor_exit` via `emit_monitor_stub` | `-> i64` | `b'V'` | yes — see §1.5 | sound |
| 17 | `inlining.rs:1135` | `getfield` (inlined callee) | `-> i64` | `type_tag` | yes | sound |
| 18 | `inlining.rs:1278` | `getstatic` (inlined callee) | `-> i64` | `type_tag` | yes | sound |
| 19 | `inlining.rs:1347` | `putstatic_*` (inlined callee) | `-> i64` | `b'V'` | yes | sound |

**Count: 19 sites, 18 sound, exactly 1 undefined-RAX guard, and it is the one
`W8-E11-1` named.** The sweep found no second instance. That is a result, not
an absence of one: the reason there is only one is that every *other* fallible
helper in this table was given an `i64` sentinel return when it was made
fallible, which is the protocol §3 extends to `aastore_check`.

The remaining `-> ()` helpers are guarded by **not being guarded at all**,
which is the correct treatment for each:

* `putfield_int/long/float/double/object` — six call sites
  (`bytecode_walk.rs:4568, 4666, 4676, 4686`, `inlining.rs:1208, 1215, 1222`,
  `objects.rs:466`) and **no exception check after any of them**. Consistent:
  they publish through the pending-NPE thread-local, drained on return.
* `write_barrier`, `satb_pre_write_barrier`, `frame_record`,
  `jit_npe_with_action`, `set_throw_bci`, `safepoint_slow_path` — infallible or
  terminal (the last two run *inside* bail stubs). No guard, correctly.
* `bastore` / `iastore` — **zero call sites in the x64 backend**; those opcodes
  are inline on every build.

### 1.3 Site 1's verdict, stated exactly

**It is not a missed exception, and it is not harmless either.**

* The exception IS delivered. `execute_jit_call`
  (`vm/src/runtime/interpreter/jit_bridge.rs:6735`) calls
  `take_all_jit_signals(thread)` on **both** the `has_dispatch` and the
  `!has_dispatch` arm, unconditionally, on every return. `jit_aastore`'s
  pending NPE / AIOOBE / stashed ASE are drained there. So the JVMS answer is
  right today. `W8-E11-1`'s "not a correctness hole" holds, and this lane
  confirms the mechanism rather than restating the claim.
* What the guard actually does is **fire at random**. RAX after a `-> ()`
  `extern "C"` call is whatever `jit_aastore`'s last internal call left there. A
  spurious hit routes to `emit_exception_check_stub`, which stamps the throw
  bci, loads `i64::MIN` and epilogues — and the drain then finds *no* signal,
  so the method returns the sentinel as its value. For a `void` method that is
  invisible; for a non-void one it cannot happen, because `aastore` cannot be
  the last instruction before a value return without a `return` in between that
  zeroes RAX (`bytecode_walk.rs:3709`, the `0xb1` arm's `XOR EAX, EAX`, whose
  own comment names this hazard). So the residual is a rare mid-method deopt,
  not a wrong answer.
* **The real cost is the comment.** `W7-38`'s own arm text said the check
  "drains the pending NPE / AIOOBE / ArrayStoreException the helper may have
  set" — it does not; the drain does. This lane rewrote that paragraph to say
  which mechanism delivers on which arm, because a guard described as the
  mechanism is precisely how the next reader deletes the real one.

§3 makes site 1 correct as a side effect, since `jit_aastore_check` returns a
real sentinel. **This lane did not "fix" site 1 by tightening the fallback arm**
— the fallback still calls a `-> ()` helper and still relies on the drain, and
the comment now says so.

### 1.4 Why the compiled-callee sites (10, 12, 13) are sound

A directly-called compiled callee returns `i64::MIN` *by construction* when it
throws or deopts (`emit_exception_check_stub`, the bounds/NPE stubs, the deopt
stubs — all `MOV RAX, i64::MIN` then epilogue). The non-throwing `void` return
is defined too: the `0xb1` arm zeroes RAX. So both directions are defined.

The related guard `emit_inline_callee_deopt_check`
(`deopt_stubs.rs:693`) — not `emit_post_invoke_exception_check`, but the same
`CMP RAX, i64::MIN` shape — carries its own comment stating that "a void callee
leaves in RAX whatever its last helper call returned", plus a bisect lever
(`CRATONVM_JIT_SP_IC_DEOPT_CHECK=void`). That comment is **stale in the
conservative direction**: the `0xb1` zeroing makes the void case defined for
any callee compiled by this backend. It over-warns; it does not under-warn.
Left alone (not this lane's call to rewrite a lever's rationale), noted here so
the next sweep does not read it as a second instance of site 1.

### 1.5 Why site 16 is sound despite an intervening emitter call

`emit_monitor_stub` (`jit/src/runtime_lowering.rs:317`) calls the monitor helper
and then `emit_post_call_frame_republish`, which either uses the inline TLS
store (no register traffic beyond RBP) or wraps its `CALL` in `PUSH RAX` /
`POP RAX`. Either way RAX survives. `jit_monitor_enter`/`jit_monitor_exit`
(`vm/src/jit/helpers.rs:4705, 4764`) both return `i64` and both return
`i64::MIN` on a null receiver, a missing thread, or an `IllegalMonitorState`.
Defined in both directions.

`emit_oop_map_for_safepoint` intervenes at sites 8, 9, 11, 14, 15, 16; its
`emit_shadow_reload` uses R10/R11 as scratch and restores operand-slot homes,
not RAX. Not examined further — it is a live-value question, not a void-guard
one.

---

## 2. TASK 2 — the atomic set, halves (c) and (d), APPLIED

### 2.1 `jit/src/x64/bytecode_walk.rs` — the `0x53` arm (half (c))

Two lowerings, selected by `self.helpers.aastore_check == 0`:

* **`0` (hand-built test tables only)** — route the WHOLE opcode to
  `helpers.aastore`, byte-identical to today. **Never** the bare inline store:
  that is the heap-type-confusion defect `W7-38` fixed.
* **wired (production)** — inline store, with the check called between the
  bounds check and the SATB barrier.

The four things `W8-E11-1` §6(c) said must be checked rather than assumed, and
what they turned out to be:

1. **`flush_scratch_registers` before every operand load.** Hoisted above the
   `if`, so both arms get it — the current arm relies on it and the inline arm
   needs it for the same reason.
2. **Re-load after every call.** The check call and the SATB call each clobber
   the caller-saved registers, so `array_slot` / `index_slot` / `val_slot` are
   re-loaded *twice*: once for the SATB old-value read, once for the store. The
   §6(c) sketch had one reload; the real sequence needs two, because the SATB
   barrier is itself a call. A version that hoists is silently wrong.
3. **`0x53` is one byte**, so `emit_post_invoke_exception_check` keeps this pc
   as the throw pc. Unchanged.
4. **RAX is a defined value on the inline arm**, so that guard now guards; on
   the fallback arm it still does not, and the comment says which is which.

The barrier/store/card-mark tail is the pre-`W7-38` sequence recovered from
`git show 7c00dee66` — `emit_ref_aload_regs` for the old value,
`satb_pre_write_barrier`, `emit_ref_astore_regs`, then
`inline_card_mark_available()` ? `emit_inline_card_mark_regs(RAX, RDX)` :
`helpers.write_barrier`. It is restored verbatim in structure, not
reconstructed from the doc comment.

### 2.2 `vm/src/jit/helpers.rs` — the initializer and the type pin (half (d))

```rust
aastore_check: jit_aastore_check as *const () as usize,
```

and, in the `const _` census block:

```rust
let _: HelperFnAastoreCheck = jit_aastore_check;
```

The second line is the whole point of the record family: it makes a signature
change to `jit_aastore_check` without a matching `helpers_abi.rs` row a **build
error** instead of a comment that quietly stops being true. `W7-38` exists
because that link was absent.

### 2.3 A correctness consequence that is NOT a throughput claim

The helper-only arm shipped by `W7-38` emits **no bounds-check stub and no
null-check stub**, because `jit_aastore` does those internally. Both vectors are
inputs to `has_dispatch` (`jit/src/x64/driver.rs:1901`:
`!compiler.bounds_check_stubs.is_empty() || !compiler.null_check_store_stubs.is_empty()`).
Nothing else on that list is implied by an `aastore`.

So a method whose only listed content is a reference array store computes
`has_dispatch == false`, takes `jit_bridge.rs:6735`'s fast entry, which skips
`set_jit_thread` — and `jit_aastore_check`'s `jit_thread_mut()` is then `None`,
so it takes its documented fail-open path and **the illegal store proceeds**.
That is the `W7-38` defect back, for that method shape.

**Reachability, narrowly and honestly.** Most `aastore`-bearing methods are
saved by something else on the `has_dispatch` list: any invoke, any `new`, any
`getstatic`, any `checkcast`, any `athrow`, and — importantly — any `arraylength`
or any *primitive* array access, which do emit null/bounds stubs. The shapes
that are NOT saved are ones where the array bound comes from elsewhere:

```java
static void put(Object[] a, int i, Object v) { a[i] = v; }
static void fill(Object[] a, int n, Object v) { for (int i = 0; i < n; i++) a[i] = v; }
```

Neither has an invoke, an allocation, a static, or an `arraylength`. This is
**PREDICTED, not measured** — no binary was built or run in this lane. The
falsifying run is in §5.

The §3 inline arm closes this structurally: it emits both stubs, which is
exactly what `driver.rs` reads. That is an argument for landing the set, and it
is deliberately kept out of the throughput ledger — §4 is unchanged and still
gates on a measurement nobody has taken.

---

## 3. NOMINATION — ATOMIC SET, the three files this lane does not own

`W8-E11-1` §6 called this a four-file set. It is **five**: the two `jit-api/`
files, the two applied here, and `jit/src/x64/tests.rs` (§3(c)), whose helper
table is an exhaustive struct literal. That fifth file is a build error, not a
test failure, and it is not in the predecessor's list.

> **THESE THREE EDITS PLUS THE TWO ALREADY APPLIED MUST LAND IN ONE COMMIT.**
> Today's tree does not compile: `jit/src/x64/bytecode_walk.rs` reads
> `self.helpers.aastore_check` and `vm/src/jit/helpers.rs` initializes it, and
> the field does not exist. Applying only (a) leaves `jit_aastore_check` dead
> code; applying only (b) fails `ABI_REVISIONS`' const assertion. There is no
> subset that builds except all four.
>
> **AND: do not land any of it until BOTH conditions in §4 are met.** They are
> gates, not advice, and they are `W7-38` §5's and `W8-E11-1` §6.5's own.

### (a) `jit-api/src/lib.rs`

**a1 — the struct field, APPENDED (the ABI is append-only; `ldc_class_cp` at
496 is currently last, so the new slot is 504 and nothing moves).** Line 1125.

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
    /// must be refused and an `ArrayStoreException` has been published on this
    /// thread, `0` when it may proceed. NOT the store: the caller keeps the
    /// inline `MOV`, the SATB pre-write barrier and the card mark.
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

**a2 — the `helper_fields!` row.** Line 1298.

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

**a3 — the pinned count.** Line 1324.

old:
```rust
    JitRuntimeHelpers::NUM_FIELDS == 63,
```
new:
```rust
    JitRuntimeHelpers::NUM_FIELDS == 64,
```

**a4 — hand-built test table #1.** Line 1716.

old:
```rust
            ldc_class_cp: 0x11B8,
        }
    }
```
new:
```rust
            ldc_class_cp: 0x11B8,
            aastore_check: 0x11C0,
        }
    }
```

**a5 — hand-built test table #2.** Line 1951.

old:
```rust
            ldc_class_cp: 0,
        };
        assert_eq!(h.newarray, 0);
```
new:
```rust
            ldc_class_cp: 0,
            aastore_check: 0,
        };
        assert_eq!(h.newarray, 0);
```

**a6 — the count assertion in `mod tests`.** Line 2128.

old:
```rust
        // And the macro-driven count is the canonical 63.
        assert_eq!(JitRuntimeHelpers::NUM_FIELDS, 63);
```
new:
```rust
        // And the macro-driven count is the canonical 64.
        assert_eq!(JitRuntimeHelpers::NUM_FIELDS, 64);
```

**a7 — the golden-offset probe row.** Line 2436.

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

`zero_field_by_name` needs **no** arm: it covers only `RequiredPtr` fields, and
`aastore_check` is optional. Adding one would be the first hint that somebody
marked the slot `required` — which it must not be, because `0` has a correct
meaning here (route to `aastore`).

### (b) `jit-api/src/helpers_abi.rs`

**b1 — the typed slot.** Line 683.

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

**b2 — the descriptor row.** Line 796.

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

**b3 — the golden offset.** Line 984.

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

**b4 — the revision ledger.** Line 1066.

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

**b5 — the version constant AND its doc line.** Lines 110 and 118.

old:
```rust
/// `4` is the revision of the 63-field, 504-byte table shipped today. The
```
new:
```rust
/// `5` is the revision of the 64-field, 512-byte table shipped today. The
```

old:
```rust
pub const JIT_HELPERS_ABI_VERSION: u32 = 4;
```
new:
```rust
pub const JIT_HELPERS_ABI_VERSION: u32 = 5;
```

**b6 — the `HELPER_FIELDS` probe list in `mod tests`.** Line 1637. *(Not in
`W8-E11-1` §6(b); found by this lane. `assert_eq!(HELPER_FIELDS.len(),
probes.len())` fails without it.)*

old:
```rust
            ("ldc_class_cp", offset_of!(H, ldc_class_cp)),
        ];
```
new:
```rust
            ("ldc_class_cp", offset_of!(H, ldc_class_cp)),
            ("aastore_check", offset_of!(H, aastore_check)),
        ];
```

**b7 — the last-golden-row assertion.** Line 1701. *(Also not in §6(b).)*

old:
```rust
        assert_eq!(last_name, "ldc_class_cp");
```
new:
```rust
        assert_eq!(last_name, "aastore_check");
```

**b8 — the ledger test's literal.** Line 1714. *(Also not in §6(b).)*

old:
```rust
            HelperAbiRevision {
                version: 4,
                num_fields: 63,
                size: 504,
            },
        );
```
new:
```rust
            HelperAbiRevision {
                version: 5,
                num_fields: 64,
                size: 512,
            },
        );
```

**b9 — the `as_words` test.** Line 2063. *(Also not in §6(b).)*

old:
```rust
        // The LAST field, whatever it currently is — `ldc_class_cp`
        // since the class-`ldc` helper was appended.
        h.ldc_class_cp = 2;
        let w = h.as_words();
        assert_eq!(w[0], 1, "first slot");
        assert_eq!(w[H::NUM_FIELDS - 1], 2, "last slot");
        assert_eq!(w.len(), 63);
```
new:
```rust
        // The LAST field, whatever it currently is — `aastore_check`
        // since the aastore check-only helper was appended.
        h.aastore_check = 2;
        let w = h.as_words();
        assert_eq!(w[0], 1, "first slot");
        assert_eq!(w[H::NUM_FIELDS - 1], 2, "last slot");
        assert_eq!(w.len(), 64);
```

Four of the nine `helpers_abi.rs` edits (b6–b9) are **not** in `W8-E11-1` §6(b).
They are all in `mod tests` and all trip at test time, not build time, so a
reader who applies §6(b) verbatim gets a green `cargo build` and four red tests.
Recorded so the next lane does not re-derive them.

### (c) `jit/src/x64/tests.rs` — the one exhaustive test literal

`test_helpers()` (line 319) is a **struct literal with no
`..Default::default()`**, so the append does not compile without it. Repo-wide
check: every other in-tree `JitRuntimeHelpers { … }` is either
`mem::zeroed()` (`ir_lower.rs:10032`) or ends in `..Default::default()`
(all thirteen `jit/tests/*.rs` builders, `jit-api/src/lib.rs:1888`). Two of
`jit-api/src/lib.rs`'s are exhaustive and are covered by a4/a5 above. This is
the only remaining one.

**The value must be `0`, not the panicking sentinel.** `0` routes the `0x53`
arm to `helpers.aastore`, which is what these tests compile against today, so
every existing `aastore` codegen test stays byte-identical. A sentinel would
silently switch them all to the inline arm and emit a `CALL` to a stub that
panics. It also matches this function's own convention for unwired optional
slots (`new_object_cp: 0`, `monitor_enter: 0`, `ldc_class_cp: 0`), each of
which carries the same comment.

old:
```rust
        // Unwired (0) — these tests build no class-`ldc` site, and 0 makes
        // the backend refuse one rather than emit a null CALL.
        ldc_class_cp: 0,
    }
}
```
new:
```rust
        // Unwired (0) — these tests build no class-`ldc` site, and 0 makes
        // the backend refuse one rather than emit a null CALL.
        ldc_class_cp: 0,
        // Unwired (0) on purpose: it routes the `0x53` arm to `aastore` (the
        // complete opcode), which is the lowering every existing `aastore`
        // codegen test in this file was written against. Wiring the panicking
        // sentinel here would switch them all to the inline-store-plus-check
        // arm and emit a CALL to a stub that panics.
        aastore_check: 0,
    }
}
```

**Consequence, stated rather than left to be discovered: with `0` here, no unit
test in `jit/src/x64/tests.rs` exercises the new inline arm.** Its cover is
`RArrayStoreTiers` under a real VM (§4 condition 1) and nothing else. A
follow-up that adds a codegen test for the inline arm needs its own helper
table with `aastore_check` wired to a counting stub — deliberately not written
here, because a codegen test asserting a byte sequence for a lowering whose
throughput has not been measured would pin a shape that may not land.

### (d) index rows — `docs/known-issues/jdk-only/INDEX.md`, line 214

**`W8-E11-1`'s own index row was nominated in its §6.6 and never applied** —
line 214 is still the last row of that table. So this nomination carries both,
in order, and whoever applies it should not be surprised to be adding two.

old:
```
| W7-38-jit-aastore-never-called-its-own-check | the JIT lowered `aastore` inline, bypassing its check | FIXED-UNVERIFIED | MIXED | after values explicitly PREDICTED |
```
new:
```
| W7-38-jit-aastore-never-called-its-own-check | the JIT lowered `aastore` inline, bypassing its check | FIXED-UNVERIFIED | MIXED | after values explicitly PREDICTED |
| W8-E11-1-jit-aastore-third-twin-and-the-check-only-helper | the JIT's ASE message named the component, not the array | FIXED-UNVERIFIED | MIXED | REACHABLE, not latent — the fixture asserts messages cold-only. Carries the ATOMIC NOMINATION SET (`W7-38` §6) gated on two measured conditions |
| W8-E19-1-the-void-guard-sweep-and-the-aastore-atomic-set | one guard in 19 tested undefined RAX; the aastore ABI set is 2/5 applied | OPEN-ATOMIC | MIXED | THE TREE DOES NOT BUILD until the three nominated halves land in the same commit. Also: IR-tier monitors miss the `has_dispatch` obligation the single-pass site carries |
```

---

## 4. The two conditions — RESTATED, NOT RELAXED

**This is not a performance win. It is a change that is READY to be measured.**

**Condition 1 — `RArrayStoreTiers` green BOTH with and without `--nojit`.**

```
cratonvm.exe --java-home $JAVA_HOME --jdk-only         -cp out RArrayStoreTiers
cratonvm.exe --java-home $JAVA_HOME --jdk-only --nojit -cp out RArrayStoreTiers
```

A single green run cannot distinguish "both tiers are right" from "the JIT never
engaged". Red without `--nojit` and green with it localises a residual to the
compiled tier; red both ways means the shared check is wrong. Add
`W8-E11-1` §4's `FastThrow` pair: with the inline store restored the ASE message
builder is reached from a *different* lowering, and `RArrayStoreTiers` asserts
messages cold-only, so it cannot see that.

**Condition 2 — measure the call first.** In `W7-38`'s own words, R20's inline
store "was a real win and this record does not have the number that would
justify re-spending it". The guess is not obviously right either way: the
check-only helper still costs a `CALL` per reference array store, plus
`element_type_of`, plus `aastore_element_assignable`, plus a class-manager read
lock; against `jit_aastore` it saves the null check, the bounds check, the
barrier dispatch and the store — real, but a fraction.

**It is entirely possible that the honest number says the four-file ABI change
is not worth it, and that is a legitimate outcome of running it.** §2.3's
`has_dispatch` consequence is a separate, correctness-side reason to want the
inline arm; if the throughput number comes back flat or negative, that argument
has to be re-made on its own terms (or the hole closed some other way), not
smuggled in behind a benchmark that did not support it.

**The benchmark** is `scratchpad/e11/AastoreCost.java` (harness-verified on
HotSpot by lane E11; **its HotSpot numbers are not the answer** — HotSpot
vectorises the `intStore` control to ~0.2 ns/store, which CratonVM will not).
Three kernels, each in its own method so each gets its own compiled site:
`refStore` (legal stores into a `String[]` seen as `Object[]` — the path that
runs billions of times), `objStore` (a genuine `Object[]`, where assignability
short-circuits; `refStore − objStore` is the hierarchy walk), `intStore` (`int[]`,
opcode `0x4f`, inline on every build — the control that removes loop / bounds /
JIT-entry overhead).

Protocol, all of it load-bearing on a shared host:

1. Three builds of the **same commit**, differing only in the `0x53` arm:
   **A** = today (whole opcode → `jit_aastore`), **B** = this set (inline store
   + `jit_aastore_check`), **C** = bare inline store, no check — *not landable*,
   it is the R20 floor, and without it "B beats A" has no scale.
2. **ABBA-interleaved**, never A-then-B, and verify the arms differ by binary
   hash before believing any gap.
3. Quote **`(refStore − intStore) / stores` in ns/store, and the A:B:C ratio**.
   Absolute wall time on a shared host is worthless.
4. The printed checksum must be identical on every arm and every rep. A build
   whose checksum differs did not do the same work.
5. Get `jit_entries` (`CRATONVM_DBG_JIT_SCAN_PROF=1`) BEFORE theorising:
   300–700 ns/entry means the kernel is call-density-bound and the `aastore`
   delta is being read out of the wrong signal.
6. Separately, `regression-suite/perf/run-cratonbench-gate.sh` against
   `cratonbench-baseline-azure-epyc.tsv`, bracketed by `reliability-gate.sh`
   PREFLIGHT/POSTFLIGHT. That answers "did this move anything else" and cannot
   answer the first question — reference array stores are a few percent of a
   mixed workload, so the gate would report even a tripled per-store cost as
   noise. Both are required; neither substitutes.

---

## 5. Verification — ALL PREDICTED

| run | predicted | what it falsifies |
|---|---|---|
| `cargo build -p cratonvm-jit` on the tree as it stands | **FAILS**: `no field aastore_check on type JitRuntimeHelpers` | that §3 really is atomic — if it builds, the field already landed elsewhere and this record is stale |
| `RArrayStoreTiers`, both ways, after §3 | `PASS (63 checks)` both | s06–s13 (`no-throw`) turning red means the inline arm creates a FALSE ASE; s14 (`NPE`) / s15 (`AIOOBE`) turning red means the NPE→AIOOBE→ASE order was disturbed — §2.1 item 2 is the likeliest cause |
| `scratchpad/e11/AseName.java` under both VMs vs `W8-E11-1` §1.1 | byte-identical | the message builder is reached from a new lowering; the fixture asserts cold-only and cannot see it |
| `FastThrow` pair (`W8-E11-1` §4) | `first == last == [Ljava.lang.Integer;`, `messageMovedAt=-1`, both runs | `messageMovedAt >= 0` on the JIT run and `-1` on `--nojit` localises a residual to the compiled tier |
| §2.3 witness: a `static void put(Object[] a, int i, Object v) { a[i] = v; }` looped hot, storing an `Integer` into a `String[]` | **before §3**: no `ArrayStoreException` once compiled, under `--jdk-only` without `--nojit`; **after §3**: throws in both runs | §2.3 is PREDICTED from reading `driver.rs:1901` + `jit_bridge.rs:6735`. If the "before" run throws, `has_dispatch` is being forced by something not on the list I read, and §2.3 is wrong |

---

## 6. TASK 3 — the single-site question, asked of every checked opcode

`W8-E11-1` §2's claim about `0x53` **re-verified independently this session**:
`ir.rs:5402` states the refusal in prose *and* omits `0x53` from its store arm's
pattern; `ir_lower.rs:4777` bails on `MemKind::Ref`; `aarch64_backend.rs:6621`
lists `0x53` in `object_model_opcodes_are_all_unsupported`;
`escape_analysis.rs:171` only analyses; `inlining.rs`'s inline-body walker has
**no array opcode arm at all** (its match jumps `0x2d → 0x36` and `0x4e → 0x57`);
and `emit_ref_astore_regs` (`arrays.rs:116`) has **zero callers** in the tree
before this lane's edit. One emission site, confirmed five ways.

Then the same question of every opcode carrying a runtime type or bounds check:

| opcode | sites | agree? |
|---|---|---|
| `checkcast` 0xc0 | **TWO** — `bytecode_walk.rs:10541` and `ir_lower.rs:5081` (`Op::CheckCast`) | yes. Same helper, same sentinel guard, and — the part that could have been missed — both force the dispatch entry (`emitted_checkcast_throw` / `ir_needs_dispatch_for_checkcast`). The IR arm's comment names the single-pass arm explicitly |
| `instanceof` 0xc1 | **TWO** — `bytecode_walk.rs:10585`, `ir_lower.rs:5111` | yes, and correctly asymmetric: `jit_instanceof` cannot throw, so neither site guards |
| `aaload` 0x32 | **TWO** — single-pass inline, and `ir_lower`'s `Op::ArrayLoad(MemKind::Ref)` | yes. Both do null-then-bounds; the IR tier deopts to the interpreter rather than throwing directly, which cannot diverge on the message |
| `athrow` 0xbf | **TWO** — `bytecode_walk.rs:3831`, `ir_lower` `Op::Throw` | yes, **now**: `lib.rs:16569` records that cov-07 shipped the IR arm *without* the `has_dispatch` obligation, measured at 1 792 397 swallowed throws in 1 800 000 iterations, and fixed it. This is the precedent for the row below |
| `monitorenter` / `monitorexit` 0xc2/0xc3 | **TWO** — `bytecode_walk.rs:10813`, `ir_lower.rs:4084` | **NO — see §6.1** |
| `aastore` 0x53 | ONE | n/a |
| `putfield` 0xb5 | THREE (`bytecode_walk`, `inlining`, `objects.rs`) | consistent: none guards, all rely on the drain (§1.2) |

### 6.1 The finding: the IR tier's monitors have no `has_dispatch` obligation

The single-pass backend sets `emitted_monitor_call = true` at its `0xc2/0xc3`
arm, and `x64/driver.rs:1911` reads it with an explicit reason:

> Live monitor helpers call `jit_thread_mut()` to identify the owner and to
> enter GC-blocked parking on contention. A monitor-only method otherwise looks
> call-free and would take the TLS-free fast entry.

The IR tier lowers the same opcodes through the same `emit_monitor_stub` to the
same helpers, and **there is no monitor arm in the IR publication path.** The
five `compiled.has_dispatch = true` assignments in `jit/src/lib.rs`
(16528, 16572, 16584, 16603, 16631) cover `Op::New`/`Op::NewArray`, `Op::Throw`,
`Op::Call` infos, `checkcast`, and `getstatic`. `grep -n has_dispatch
jit/src/ir_lower.rs` returns **nothing**.

Consequence if reached: `jit_thread_mut()` is `None`, both monitor helpers take
their `let Some(...) else { return i64::MIN }` arm and return the sentinel
**without publishing any exception**. The IR arm's own `CMP RAX, i64::MIN`
(`ir_lower.rs:4105`) then jumps to the shared exception epilogue, the drain
finds nothing, and the method returns as if it completed — **with the lock never
taken.** That is worse than `athrow`'s swallowed-throw, because it is a lock.

**Why nothing has been observed, and why that is exactly the shape being
reported.** javac compiles `synchronized (o) { … }` with a synthetic
catch-all handler ending in `athrow`, so every javac-emitted synchronized block
carries an `Op::Throw` — and `Op::Throw` has its own `has_dispatch` arm.
**The monitor site is correct only by accident of a second mechanism it does not
name and does not depend on.** Break the coincidence — an ASM/ECJ/Kotlin-emitted
monitor pair without an `athrow`, or any future change that lowers that handler
differently — and the lock silently disappears. `ACC_SYNCHRONIZED` methods are
not affected: they carry no monitor bytecode at all.

**This is a NOMINATION, not a fix.** `jit/src/lib.rs` is not this lane's file,
and it is PREDICTED from reading the three files above — not measured, and not
witnessed by a program. The minimal edit, next to the `Op::Throw` arm at
`lib.rs:16569`:

old:
```rust
                        if graph.nodes.iter().any(|n| matches!(n.op, ir::Op::Throw)) {
                            compiled.has_dispatch = true;
                        }
```
new:
```rust
                        if graph.nodes.iter().any(|n| matches!(n.op, ir::Op::Throw)) {
                            compiled.has_dispatch = true;
                        }
                        // Same obligation, same reason, for monitors — the arm
                        // this tier did not bring across when it admitted
                        // `monitorenter`/`monitorexit`. `jit_monitor_enter` /
                        // `jit_monitor_exit` resolve the owning thread through
                        // `jit_thread_mut()`, which only the dispatch-aware
                        // entry sets; without it BOTH return `i64::MIN` having
                        // published no exception, the lowering's own sentinel
                        // check jumps to the exception epilogue, the drain
                        // finds nothing, and the method returns with the lock
                        // NEVER TAKEN. The single-pass backend has carried
                        // this as `emitted_monitor_call` (`x64/driver.rs`).
                        //
                        // Not observed to date only because javac emits a
                        // catch-all `athrow` cleanup handler for every
                        // `synchronized` block, so the arm above fires first.
                        // That coincidence is not a guarantee: hand-emitted or
                        // non-javac bytecode with a bare monitor pair has no
                        // `Op::Throw`.
                        if graph
                            .nodes
                            .iter()
                            .any(|n| matches!(n.op, ir::Op::MonitorEnter | ir::Op::MonitorExit))
                        {
                            compiled.has_dispatch = true;
                        }
```

Falsifying test before landing it: a hand-assembled class with
`aload_0; monitorenter; aload_0; monitorexit; return` and **no** exception
table, driven hot. Predicted before: the lock is not taken (observable as a lost
increment under contention, or `IllegalMonitorStateException` in the interpreted
caller). Predicted after: correct. If the "before" run behaves correctly, the
method is not reaching the IR tier and this nomination is wrong — check
`CRATONVM_DBG_JIT` for which backend compiled it before blaming the reasoning.

---

## 7. Residual

* **The tree does not build.** Stated three times on purpose. §3 or revert.
* **§2.3 and §6.1 are both PREDICTED from source.** Neither has a witness
  program run against a binary. Each names its own falsifying run.
* **`emit_inline_callee_deopt_check`'s "void callee leaves garbage in RAX"
  comment is stale in the safe direction** (§1.4). Left alone.
* **`array_descriptor_of` still fails soft to `[Ljava/lang/Object;`** when the
  component class cannot be resolved (`W8-E11-1` §5). Unchanged, now shared by
  a second lowering of the same opcode.
* **The fixture still cannot see message fixes in the hot tier.** `W8-E11-1` §7.
  Restoring the inline store does not change that, and makes a second lowering
  depend on it.
