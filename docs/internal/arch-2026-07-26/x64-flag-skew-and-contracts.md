# `jit/src/x64.rs` — flag skew and codegen/runtime contracts

*Session slug: `x64-flag-skew-and-contracts`. Written against `dev` @ `6495a191c`
(2026-07-26). Scope of edits: `jit/src/x64.rs` only. Everything under §7 is a
request to another owner and was **not** edited here.*

Companion reading:

- `docs/internal/flag-census.md` — the workspace-wide flag inventory this work sits in.
- `docs/known-issues/moving-young-gen-drops-jit-held-oops.md` — **OPEN**. Do not flip
  moving-young on the strength of anything in this document.
- `docs/feature-designs/default-moving-young-gen.md`
- `docs/internal/inline-allocation-and-reference-publication.md`

---

## 1. What was actually wrong

`CRATONVM_MOVING_YOUNG` was parsed **three times, independently**:

| Crate | Site | State on `6495a191c` before this change |
| --- | --- | --- |
| `gc` | `gc/src/gc_quiescence.rs::moving_young_enabled` | already centralized (`gc_flags().moving_young`) |
| `jit` | `jit/src/x64.rs::moving_young_enabled` | crate-private `OnceLock` + `getenv` |
| `vm` | `vm/src/jit/conservative_roots.rs::moving_young_enabled` | crate-private `OnceLock` + `getenv` |

The same shape held for `CRATONVM_SHADOW_STACK` (`gc/src/gen_heap.rs:3684` already read
`cratonvm_types::flags().jit.shadow_stack`; `jit` and `vm` still `getenv`'d).

This is not a style problem. The two halves of moving-young are only sound *together*:
the JIT must publish a **complete rewritable** precise root map at every GC-capable
safepoint, and the collector must run the moving (Cheney) cycle with the conservative
frame scan suppressed. If codegen and the collector disagree about the gate — because
one crate latched its `OnceLock` before a launcher `install()`, or because a future flip
of the default lands in `flags.rs` but not in a private `getenv` — the collector
relocates objects whose JIT-held references were never published. That is heap
corruption, not a wrong answer.

**Today the two spellings are behaviourally identical**: `flags.rs` builds
`gc.moving_young` with `parse::present`, which is literally `src.get(name).is_some()` —
the same predicate as `std::env::var_os(..).is_some()`. So the de-skew is a no-op at
runtime and is safe to land on its own. Its value is that it makes the *next* change —
flipping a default — a one-line edit in one file instead of a three-crate archaeology
exercise.

## 2. Task 1 — landed

`jit/src/x64.rs`:

```rust
#[inline]
pub fn moving_young_enabled() -> bool {
    cratonvm_types::flags().gc.moving_young
}
```

The `OnceLock` is gone entirely — `flags()` is itself a `OnceLock` and `#[inline]` on a
single field read is cheaper than the old double-checked load. `shadow_stack_maps_enabled`
was converted in the same way (it keeps its own `OnceLock` because it ORs in
`moving_young_enabled()`):

```rust
*G.get_or_init(|| cratonvm_types::flags().jit.shadow_stack || moving_young_enabled())
```

**No default was changed.** Moving-young remains off, and remains unsound: the OPEN
known-issue records Binary Trees emitting `68310832` against the correct `68332206` when
the flag is forced on with the JIT active.

Tests added (`mod flag_and_header_contracts` at the end of `x64.rs`):

- `moving_young_enabled_is_the_centralized_flag`
- `shadow_stack_maps_enabled_is_central_flag_or_moving_young`
- `no_crate_private_getenv_for_centralized_gates` — an `include_str!` source scan that
  fails if either variable is ever `getenv`'d from this file again. Needles are assembled
  at runtime so the test's own text does not match them.

---

## 3. Task 2 — full audit of `std::env::var*` in `x64.rs`

67 direct reads remain after the two conversions (69 before). Classification:

- **(a) pure diagnostic** — gates only `eprintln!`; deleting it cannot change a
  program's result. **24 sites / 12 distinct variables.** Leave alone.
- **(b) duplicates a centralized flag** — must read `cratonvm_types::flags()`.
  **2 sites / 2 variables, both fixed.**
- **(c) semantics-affecting, not yet centralized** — selects a different code path.
  **43 sites / 39 distinct variables.** A `flags.rs` field is proposed for each below.

24 + 43 = 67 remaining direct reads, which reconciles with the raw `env::var` scan of the
edited file (excluding the source-scan regression test's own runtime-assembled needles).

### 3.1 Category (b) — fixed

| Was | Now | Field |
| --- | --- | --- |
| `x64.rs:2458` `var_os("CRATONVM_MOVING_YOUNG")` | `flags().gc.moving_young` | `GcFlags::moving_young` |
| `x64.rs:2436` `var_os("CRATONVM_SHADOW_STACK")` | `flags().jit.shadow_stack` | `JitFlags::shadow_stack` |

These were the only two: a name-set intersection of every `CRATONVM_*` literal in
`x64.rs` against every one in `types/src/flags.rs` returns exactly `{CRATONVM_MOVING_YOUNG,
CRATONVM_SHADOW_STACK}`.

### 3.2 Category (a) — pure diagnostic, leave alone

Line numbers are post-edit.

| Variable | Sites | Gates |
| --- | --- | --- |
| `CRATONVM_DBG_JITC` | 1721, 28743 | scan-bail / compile trace |
| `CRATONVM_DBG_INLINE_FR` | 2389 | inline frame-record probe trace |
| `CRATONVM_DBG_SHADOW2` + `_FILTER` | 2491, 2494 | shadow diag, method-name filter |
| `CRATONVM_DBG_SHADOW_RELOAD` | 2507 | emits a bad-path-only logging **call** (see note) |
| `CRATONVM_DBG_DEOPT` | 9310, 28112, 28506 | deopt/LICM trace |
| `CRATONVM_DBG_SCALAR_DEOPT` | 9465, 19270, 28390, 28622 | scalar-replacement trace |
| `CRATONVM_DBG_SPID` | 10034, 10064 | safepoint-id slot trace |
| `CRATONVM_DBG_COMPACT_INLINE` | 15244, 22076, 22540 | compact-layout inline trace |
| `CRATONVM_DBG_JIT_GEN` | 26252, 28131, 28635, 28654 | codegen trace |
| `CRATONVM_DBG_OSR_META` | 28982 | OSR metadata dump |
| `CRATONVM_DBG_VERIFY_INLINE_FRAME_RECORD` | 2313 | emits a verify **call** (see note) |

Note on the two starred cases: `CRATONVM_DBG_SHADOW_RELOAD` and
`CRATONVM_DBG_VERIFY_INLINE_FRAME_RECORD` are diagnostic in *intent* but they change the
emitted instruction stream (they add a helper CALL). They cannot change a correct
program's result — the helpers only read and print — but they are not byte-identical, so
an A/B that uses them is not measuring the default path. Classified (a) because the
observable Java-level semantics are unchanged; flagged here so nobody uses them as a
perf baseline.

### 3.3 Category (c) — semantics-affecting, not yet centralized

Every one of these selects a different code path. None of them is currently in
`flags.rs`. Proposed field names assume a `JitFlags` expansion (`flags.rs` already notes
"the rest arrive with the `jit` migration"). Ordered by GC-correctness risk.

#### Tier 1 — GC root-coverage gates (a wrong answer here is a dangling oop)

| Variable | Sites | Default | Proposed `JitFlags` field |
| --- | --- | --- | --- |
| `CRATONVM_NO_PRECISE_JIT_MAPS` | 2097 | maps **ON** | `no_precise_maps: bool` (opt-out) |
| `CRATONVM_NO_PRECISE_INLINE_FRAME_RECORD` | 2300 | inline FR **ON** | `no_precise_inline_frame_record` |
| `CRATONVM_JIT_SAFEPOINT_REG_SPILL` | 2639, 2650, 2669 | tri-state, see §4 | `safepoint_reg_spill: RegSpillMode` |
| `CRATONVM_NO_PRECISE_REG_SPILL` | 2690 | fold **ON** | `no_precise_reg_spill` |
| `CRATONVM_JIT_NO_CALLEE_OOP_FLUSH` | 2733 | flush **ON** | `no_callee_oop_flush` |
| `CRATONVM_JIT_FULL_SELF_CALL_SPILL` | 2699 | off | `full_self_call_spill` |
| `CRATONVM_JIT_SAFEPOINT_POLLS` | 2624 | polls **ON** (`=0` opts out) | `safepoint_polls` |
| `CRATONVM_JIT_ENABLE_CALLEE_SAVED_GPR_LOCALS` | 2752 | on, **and**ed with precise-maps | `callee_saved_gpr_locals` |

`CRATONVM_JIT_SAFEPOINT_REG_SPILL` deserves a real enum rather than three
`OnceLock<bool>`s that each re-read the same variable and compare it against a different
literal (`is_some()` / `"nostore"` / `"all"`). A `RegSpillMode { Off, CalleeSaved, NoStore, All }`
parsed once removes the possibility of `=nostore` being read as "on" by one predicate and
"nostore" by another — which is exactly what happens today, deliberately, and is easy to
get wrong when editing.

#### Tier 2 — allocation and field-access path selection

| Variable | Sites | Default | Proposed field |
| --- | --- | --- | --- |
| `CRATONVM_NO_JIT_INLINE_TLAB_NEW` | 2161 | inline TLAB **ON** | `no_inline_tlab_new` |
| `CRATONVM_NO_JIT_INLINE_PUTFIELD` | 2138 | inline putfield **ON** | `no_inline_putfield` |
| `CRATONVM_JIT_INLINE_GETFIELD` | 2186 | off (raw, unguarded) | `inline_getfield_raw` |
| `CRATONVM_JIT_GETFIELD_HELPER` | 2243 | guarded inline **ON** | `getfield_helper_only` |
| `CRATONVM_JIT_DISABLE_INLINE_NEW` | 27270, 28422 | inline `new` allowed | `disable_inline_new` |
| `CRATONVM_JIT_ENABLE_INLINE_NEW` | 27273, 28417 | off (force) | `force_inline_new` |

`guarded_inline_getfield_enabled` (2243) is **deliberately not** `OnceLock`-cached; its
doc comment explains why (compile-time-only gate; caching would make the off-switch racy
against whichever thread first triggers a `getfield` compile). Centralizing it into
`flags()` preserves that property — `flags()` latches once at process start, before any
compilation, which is strictly better than "whichever compile ran first".

#### Tier 3 — optimizer / codegen shape

| Variable | Sites | Default | Proposed field |
| --- | --- | --- | --- |
| `CRATONVM_JIT_NO_STACK_BANG` / `CRATONVM_JIT_STACK_BANG` | 192, 195 | bang **ON** | `stack_bang` (one field, two spellings) |
| `CRATONVM_JIT_NO_DUPX` | 1076 | dup_x **ON** | `no_dupx` |
| `CRATONVM_JIT_NO_DUP_X1` | 1083 | on | `no_dup_x1` |
| `CRATONVM_JIT_NO_DUP_X2` | 1089 | on | `no_dup_x2` |
| `CRATONVM_JIT_DUPX_EAGER_CANON` | 1100 | off | `dupx_eager_canon` |
| `CRATONVM_JIT_INLINE_SELF_GUARD` | 2261 | on | `inline_self_guard` |
| `CRATONVM_JIT_NO_SELF_CACHE_INHERIT` | 2274 | inherit **ON** | `no_self_cache_inherit` |
| `CRATONVM_JIT_KERNEL_REG_LOCALS` | 2796 | on | `kernel_reg_locals` |
| `CRATONVM_JIT_KERNEL_REG_OSR` | 2874 | off | `kernel_reg_osr` |
| `CRATONVM_JIT_NO_SLOT_MIRROR` | 2896 | mirror **ON** | `no_slot_mirror` |
| `CRATONVM_JIT_INCLUSIVE_BCE` | 6176 | — | `inclusive_bce` |
| `CRATONVM_JIT_NO_SPEC_BCE` | 6188 | spec BCE **ON** | `no_spec_bce` |
| `CRATONVM_JIT_NO_BCE` | 28187 | BCE **ON** | `no_bce` |
| `CRATONVM_DISABLE_AALOAD_LICM` | 28097 | LICM **ON** | `disable_aaload_licm` |
| `CRATONVM_DISABLE_ARITH_LICM` | 28126 | on | `disable_arith_licm` |
| `CRATONVM_DISABLE_UNROLL` | 28252 | unroll **ON** | `disable_unroll` |
| `CRATONVM_DISABLE_SCALAR_REPLACEMENT` | 28403 | SR **ON** | `disable_scalar_replacement` |

#### Shadow-stack bisection toggles (semantics-affecting, but scaffolding)

`CRATONVM_SHADOW_NOPUSH` (2530), `_NORELOAD` (2540), `_PIN` (2554), `_SENTINEL` (2563),
`_WATCH` (2572), `_RAW_RELOAD` (2601), `_NO_SAVEBASE` (2612). Seven variables, seven
`OnceLock<bool>`s, all off by default, all only meaningful when the shadow stack is on —
which is itself experimental and must stay off. These change emitted code, so they are
category (c), but centralizing them individually is low value. **Recommendation:** fold
them into a single `JitFlags::shadow_bisect: ShadowBisect` bitflags field, or retire the
family with the shadow-stack scaffolding when moving-young is either landed or abandoned.
They are the largest single cluster of dead-weight gate surface in this file.

### 3.4 Nothing else in `x64.rs` reads a centralized flag by hand

Beyond the two fixed sites, `x64.rs` already consumes shared state through typed APIs:
`cratonvm_types::narrow_oop::narrow_oops_enabled()`,
`cratonvm_types::compact_ref_fields_enabled()`, `cratonvm_types::class_layout()`,
`cratonvm_types::layout_replace_guard()`. Those are correct and were left alone.

---

## 4. The safepoint register-spill fold — **verified genuinely in place**

The concern was that the "several call sites claim a protection that never runs on the
default path" period might still be live. It is not. On `6495a191c`, at
`x64.rs:8699-8729` (`Compiler::new`):

```rust
let precise_maps = precise_jit_maps_enabled() || moving_young_enabled();
...
let precise_implies_reg_spill = precise_maps && !precise_reg_spill_disabled();
let safepoint_reg_spill     = safepoint_reg_spill_enabled() || precise_implies_reg_spill;
let safepoint_reg_spill_all = safepoint_reg_spill_all()     || precise_implies_reg_spill;
let safepoint_reg_spill_nostore = safepoint_reg_spill_nostore();
```

Chasing the defaults through:

- `precise_jit_maps_enabled()` = `var_os("CRATONVM_NO_PRECISE_JIT_MAPS").is_none()` → **true** by default (2097).
- `precise_reg_spill_disabled()` = `var_os("CRATONVM_NO_PRECISE_REG_SPILL").is_some()` → **false** by default (2690).
- ⇒ `precise_implies_reg_spill` = **true** by default.
- ⇒ both `safepoint_reg_spill` and `safepoint_reg_spill_all` are **true** by default,
  independently of `CRATONVM_JIT_SAFEPOINT_REG_SPILL`.

And the spill actually emits: `emit_pre_safepoint_spill` (`x64.rs:9980`) guards on
`self.safepoint_reg_spill && !self.safepoint_reg_spill_nostore && self.reg_spill_base != 0`
(`x64.rs:10005`), then widens to the full GPR file under `self.safepoint_reg_spill_all`
(`x64.rs:10014`). `reg_spill_base` is non-zero because `reg_spill_size` (`x64.rs:8881`)
reserves `ALL_SPILL_GPRS.len()` slots on the `_all` arm. `safepoint_reg_spill_nostore` is
the `=nostore` diagnostic and is false unless explicitly set.

**Conclusion: the default path spills the full GPR file at GC-capable safepoints. The
historical hole is closed. No residue.** The in-file comment at `x64.rs:2663-2674` is an
accurate description of a *past* state, not a current one; it is worth keeping because it
explains why the fold exists.

One subtlety worth recording: the inverted opt-out is `CRATONVM_NO_PRECISE_REG_SPILL`,
**not** a value on `CRATONVM_JIT_SAFEPOINT_REG_SPILL`. Setting
`CRATONVM_JIT_SAFEPOINT_REG_SPILL=0` does **not** disable the spill — `..._enabled()` is
`is_some()`, so `=0` *enables* the callee-saved arm, and the `precise_implies_reg_spill`
term keeps `_all` on regardless. Anyone bisecting must use
`CRATONVM_NO_PRECISE_REG_SPILL=1`. This is a real foot-gun and is another argument for the
`RegSpillMode` enum proposed in §3.3.

Also note the soundness argument the `=all` path rests on (`x64.rs:2656-2664`): full-GPR
blind spilling is conservative and "can only over-retain, never corrupt" **because the
young sweep is non-moving while any thread is in JIT** (`gc_quiescence`; the argument text
is at `x64.rs:2646-2665`). That premise is
exactly what moving-young removes. If moving-young is ever flipped on, this spill stops
being safe-by-over-retention and every spilled slot becomes a relocation candidate that
must be precisely typed. Do not treat the fold as pre-validating moving-young.

---

## 5. Contract A — the inline TLAB allocation sequence

**Emitter:** `Compiler::emit_inline_tlab_new`, `jit/src/x64.rs:14997-15342`.
**Rust counterpart:** `Tlab::alloc_initialized`, `gc/src/tlab.rs:249-292` (owner: the `gc`
sibling). **Gate:** `inline_tlab_new_enabled()` (`x64.rs:2161`), default **ON**; opt out
with `CRATONVM_NO_JIT_INLINE_TLAB_NEW=1`, which routes every `new` through
`helpers.new_object`.

### 5.1 Field offsets are negotiated at runtime, not baked from a Rust struct

The emitter does **not** hardcode `Tlab` field offsets. It reads them from the helpers
table:

- `helpers.tlab_cursor_offset_in_thread`
- `helpers.tlab_end_offset_in_thread`
- `helpers.class_id_offset_in_obj` (0 by contract)

`vm/src/jit/helpers.rs:9756-9758` computes them as
`JvmThread::tlab_offset() + Tlab::CURSOR_OFFSET` / `+ Tlab::END_OFFSET`, pinned by
`Tlab::test_tlab_offsets` and `JvmThread::tlab_offset_matches_field_address`. This half of
the contract is sound and needs no further work. `Tlab::start` and `Tlab::pressure` are
never read by generated code.

### 5.2 The emitted sequence, exactly

Registers: `R10` = `JvmThread*`, `R11` = object base, `RAX` = new cursor / result.

```
0.  [compact classes only] layout-replace guard:
      mov  r11, imm64(layout_replace_counter_addr)
      mov  eax, [r11]
      cmp  eax, imm32(counter_at_compile_time)
      jne  slow_path                      ; layout was REPLACED → helper
1.  thread fetch: cached frame slot if `jit_thread_slot_off != 0`,
      else `call helpers.get_current_thread`;  test rax,rax; je slow_path
2.  mov  r10, rax
    mov  r11, [r10 + cursor_off]          ; load cursor
3.  add  r11, 7
    and  r11, -8                          ; align UP to 8
4.  lea  rax, [r11 + total_size]          ; prospective new cursor
5.  cmp  rax, [r10 + end_off]
    ja   slow_path                        ; TLAB exhausted
6.  BODY ZEROING — xor-free: `mov rdx, 0`, then one
      `mov qword [r11 + off], rdx` per 8 bytes, off in HEADER_SIZE..total_size
7.  HEADER WRITES (all dword immediates):
      [r11 + 0]                    = class_id_raw
      [r11 + OBJECT_KIND_OFFSET=4] = 0        ; kind=Object, elem=Reference, gc_age, gc_flags
      [r11 + IDENTITY_HASH=8]      = 0        ; lazy-mint contract
      [r11 + NUM_SLOTS_OFFSET=12]  = num_fields
      [r11 + 4]                    = GC_FLAG_COMPACT << 24   ; compact classes only
      [r11 + FORWARDING_PTR=16]    = 0
      [r11 + 20]                   = 0        ; forwarding_ptr upper dword
      [r11 + MARK_WORD=24]         = 0        ; MARK_NEUTRAL
      [r11 + 28]                   = 0        ; mark_word upper dword
8.  CURSOR COMMIT — the single linearization point:
      mov  [r10 + cursor_off], rax
9.  either `mov rax, r11` (skip_post_init_helper) or
      `call helpers.tlab_post_init(vm, obj, cid, nf)`
10. jmp done
    slow_path:  call helpers.new_object(heap, cid, nf)
    done:       both arms converge with RAX = object pointer
```

`total_size = HEADER_SIZE + (compact_body_size | num_fields * SLOT_SIZE)`, computed at
JIT-compile time and baked as an immediate.

### 5.3 Order of header write vs cursor commit — what the code actually does

**The full object header and the zeroed body are written BEFORE the cursor commit.** Step
8 is the only store that makes the object reachable to a heap walk, and on x86-64 TSO
stores are not reordered with older stores, so no walker can observe a
committed-but-unheadered object. The emitter does **not** emit an `SFENCE`, deliberately
(`x64.rs:15288-15295`).

This matters because two contradictory claims are in circulation and both are partly
right:

- A July 2026 Binary Trees regression **was** traced to two independent causes, one of
  them inline-TLAB-related. That is true, and the current shape (default-ON with every
  header field written explicitly) is the *fix*, described at `x64.rs:2141-2163`.
- A separate investigation concluded the suspected **"header written before cursor
  commit" defect does not exist**. Also true — and the reason it does not exist is that
  the ordering was already corrected (`x64.rs:15170-15210` documents the original
  mis-ordering and its symptom: a walker computing
  `size = HEADER_SIZE + 0*SLOT_SIZE` and stepping into the object's own field region,
  decoding `class_id=4, array_length=1, num_slots=384`, then desyncing → `rc=124` timeout
  on `bintrees18`).

So: the ordering is correct **today**, the historical bug was real, and the "defect
doesn't exist" finding is a statement about the current tree, not a claim that the
ordering never mattered. Anyone touching step 7 or 8 must preserve their relative order.

A second, independent hardening is in force: **no header field relies on TLAB-refill
zeroing.** That assumption was empirically violated twice (offset 4 observed as
`0x01010101` from prior `byte[]` data — ECJ `HashtableOfInt` `/by zero`, and the bt18
case), so `forwarding_ptr` and `mark_word` are now written as explicit dword pairs even
though both are zero.

### 5.4 Divergences from `Tlab::alloc_initialized` — read this before touching either side

| Aspect | JIT (`emit_inline_tlab_new`) | Rust (`alloc_initialized`) | Assessment |
| --- | --- | --- | --- |
| Cursor align-up | `add 7; and -8` (hardcoded 8) | `(cursor + align-1) & !(align-1)`, caller passes `align` | Equivalent while every `new` uses `align = 8`. The JIT cannot express any other alignment. |
| Reserved footprint | `new_cursor = aligned + total_size`, **not** rounded up | `footprint = size rounded UP to align`; `new_cursor = aligned + footprint` | **Genuine divergence.** For legacy layouts `total_size` is already a multiple of 16 so they coincide. For **compact** bodies (`body_size` need not be 8-aligned) the JIT publishes a cursor that can be non-8-aligned where the Rust path would have rounded. Both the JIT's own next allocation and any subsequent Rust `alloc_initialized` re-align on entry, and `install_tail_filler` re-aligns too, so no object is misplaced — but the *published cursor value* differs between the two allocators for the same allocation. Any code that assumes `Tlab::cursor` is 8-aligned is wrong under the JIT path. |
| Publication barrier | none — relies on x86-64 TSO | explicit `fence(Release)` | Sound. The Rust fence is primarily a *compiler* barrier; machine code has no compiler to reorder it. Do not "fix" this by emitting `SFENCE`. |
| Overflow handling | `ja slow_path` → `helpers.new_object` (full allocator, incl. refill) | returns `None`, caller refills | Equivalent. |
| Allocation-pressure accounting | **not updated** | `pressure.allocations_since_last_refill += size`, `alloc_count += 1`, `large_alloc_count` | **Genuine divergence, and the sharper one.** `jit_post_tlab_init` (`vm/src/jit/helpers.rs:2226+`) does not touch `pressure` either, and the `skip_post_init_helper` fast path does not even call it. So JIT-inline allocations are invisible to the TLAB pressure tracker that drives GC scheduling and refill sizing. On an allocation-heavy JIT'd workload — precisely the workload this path exists for — the tracker under-counts by close to 100%. See §7 request R2. |
| Layout staleness | 3-instruction `layout_replace_guard` diverts to the helper if the class's compact layout was replaced after compile | reads the current layout per allocation | JIT is correct-by-diversion. |

---

## 6. Contract B — object-header offsets baked into codegen

**Do not make this change. This section is the map for whoever does.**

Target: shrink `ObjectHeader` from 32 bytes to 16 by folding `forwarding_ptr` and
`identity_hash_code` into the mark word.

### 6.1 Current layout (`types/src/heap_types.rs`)

| Offset | Field | Named constant | Size |
| ---: | --- | --- | ---: |
| 0 | `class_id` | *(none — 0 by JIT contract)* | 4 |
| 4 | `kind` | `OBJECT_KIND_OFFSET` | 1 |
| 5 | `element_type` | `ARRAY_ELEMENT_TYPE_OFFSET` | 1 |
| 6 | `gc_age` | `GC_AGE_OFFSET` | 1 |
| 7 | `gc_flags` | `GC_FLAGS_OFFSET` | 1 |
| 8 | `identity_hash_code` | *(none in `types`; `IDENTITY_HASH_CODE_OFFSET` now derived locally in `x64.rs`)* | 4 |
| 12 | `shape` (array length **or** instance-field count) | `ARRAY_LENGTH_OFFSET` **==** `NUM_SLOTS_OFFSET` | 4 |
| 16 | `forwarding_ptr` | `FORWARDING_PTR_OFFSET` | 8 |
| 24 | `mark_word` | `MARK_WORD_OFFSET` | 8 |
| | | `HEADER_SIZE = 32` | |

`HEADER_SIZE <= 127` is asserted in `heap_types.rs:24-27` because the JIT emits array
element offsets as a **signed** `disp8`. Note the assert's comment cites
`jit/src/x64.rs:6690-6813` as the range to convert — **that range is stale**; on
`6495a191c` it lands in loop/BCE analysis, not in any emitter. See §7 request R3.

### 6.2 Site inventory in `jit/src/x64.rs`

Counts are pinned by `header_offset_emission_site_inventory_matches_the_doc`; if that test
fails, this table is stale.

| Pattern | Count | Encoding | Line numbers |
| --- | ---: | --- | --- |
| `HEADER_SIZE as u8` via `buf.emit_byte(..)` | 17 | ModRM **disp8** | 14608, 14632, 17106, 17117, 17129, 17138, 17156, 17175, 17201, 17223, 17234, 17248, 17260, 17272, 17283, 20078, 20112 |
| `HEADER_SIZE as u8` inside a literal instruction byte array | 14 | **disp8** / imm8 | 23746, 23755, 23981, 24104, 24107, 24232, 25130, 25142, 25199, 25211, 25366, 25369, 26068, 26086 |
| `(HEADER_SIZE as i32).to_le_bytes()` | 10 | **disp32** / imm32 | 13063, 13144, 13199, 13285, 13580, 13588, 13596, 13674, 13680, 13689 |
| `HEADER_SIZE as i32` (bare) | 1 | disp32 | 25881 |
| `HEADER_SIZE` in compile-time arithmetic | 18 | not emitted directly | 64 (import), 14880, 14959, 15070, 15164, 15166, 22109, 22110, 22313, 22552, 22641, 27247, 28426, 29437, 29461, 29476, 29491, 29507, 29529 |
| `ARRAY_LENGTH_OFFSET as u8` | 16 | **disp8** | 17289, 17452, 18886, 18976, 23706, 23716, 23968, 24064, 24068, 24234, 25094, 25175, 25220, 25351, 25354, 26042 |
| `ARRAY_LENGTH_OFFSET as i32` | 5 | disp32 | 25497, 25503, 25607, 25723, 25728 |
| `NUM_SLOTS_OFFSET as i32` | 4 | disp32 | 14915, 15239 (inline TLAB `shape`), 22614, 22689 |
| `GC_FLAGS_OFFSET as i32` | 11 | disp32 | 14671, 14690, 14890, 14900, 14965, 14972, 22144, 22351, 22591, 22600, 22669 |
| `FORWARDING_PTR_OFFSET as i32` (and `+ 4`) | 2 | disp32 | 15269, 15274 |
| `MARK_WORD_OFFSET as i32` (and `+ 4`) | 2 | disp32 | 15279, 15284 |
| `OBJECT_KIND_OFFSET as i32` | 2 | disp32 | 15231 (was bare `4`), 15255 (compact flag, was bare `4`) |
| `IDENTITY_HASH_CODE_OFFSET as i32` | 1 | disp32 | 15234 (was bare `8`) |
| `NUM_SLOTS_OFFSET` in Rust pointer arithmetic | 1 | n/a | 29422 |

Totals: **31** `HEADER_SIZE` disp8 sites, **11** `HEADER_SIZE` disp32 sites, **18**
compile-time-arithmetic uses, **21** `ARRAY_LENGTH_OFFSET` sites, **23** other named
header-offset sites. **104 sites** in this file.

The four rows marked "was bare" are the whole of this session's mechanical change to §6:
three bare integer literals (`4`, `8`, `4`) inside `emit_inline_tlab_new` became named
constants, and the `<< 24` compact-flag shift became
`<< (8 * (GC_FLAGS_OFFSET - OBJECT_KIND_OFFSET))`. No offset *value* changed.

### 6.3 What the shrink actually has to deal with

1. **`HEADER_SIZE` shrinking 32 → 16 is value-safe for every disp8 site.** 16 still fits a
   signed byte. Those 31 sites need no encoding change; they recompile correctly.
2. **`ARRAY_LENGTH_OFFSET` / `NUM_SLOTS_OFFSET` is the harder surface.** It is `12` today
   and *will* move in a 16-byte header (`class_id`(4) + kind-word(4) + `shape`(4) = 12
   leaves only 4 bytes, so the mark word cannot fit at 16 unless `shape` moves). 21 sites
   bake it, 16 of them as disp8. All are mechanical **provided** the new offset still fits
   disp8 — it will, but nothing asserts it. The `#[cfg(test)]`
   `header_size_fits_signed_disp8_and_is_qword_aligned` added this session now asserts
   `i8::try_from(ARRAY_LENGTH_OFFSET).is_ok()` so the shrink trips a test rather than
   emitting a negative displacement.
3. **The compact-flag store is the only site with a non-trivial encoding dependency.**
   `emit_inline_tlab_new` writes `GC_FLAG_COMPACT` as a whole dword at `OBJECT_KIND_OFFSET`
   with the flag shifted into byte 3. That is only correct while
   `GC_FLAGS_OFFSET - OBJECT_KIND_OFFSET == 3`. It was a bare `<< 24` against a bare `4`;
   it is now expressed as `<< (8 * (GC_FLAGS_OFFSET - OBJECT_KIND_OFFSET))` and pinned by
   `header_offset_contract_gc_flags_is_byte3_of_kind_dword`.
4. **`FORWARDING_PTR_OFFSET` and `MARK_WORD_OFFSET` are written as dword *pairs*** (base
   and base+4) because there is no qword-immediate store emitter. Folding
   `forwarding_ptr` into the mark word deletes 2 of those 4 stores; the remaining pair
   must still cover the full 8 bytes.
5. **`identity_hash_code` had no named constant in `types`.** `x64.rs` now derives
   `IDENTITY_HASH_CODE_OFFSET` from `offset_of!(ObjectHeader, identity_hash_code)`, so it
   cannot silently drift. `vm/src/jit/helpers.rs` still writes it as a bare
   `raw_ptr.add(8)` — see §7 request R3.
6. **`class_id` at offset 0 is load-bearing beyond this file** — `jit-api` documents it as
   "0 by contract" and `helpers.class_id_offset_in_obj` is hard-set to 0. It must stay at 0.

---

## 7. Cross-owner requests (NOT edited here)

### R1 — `vm/src/jit/conservative_roots.rs`: finish the three-way de-skew

Owner: the `vm` sibling. Two crate-private `getenv`s remain and are now the **only**
un-centralized readers of these gates:

- `vm/src/jit/conservative_roots.rs:377` — `moving_young_enabled()` should be
  `cratonvm_types::flags().gc.moving_young`.
- `vm/src/jit/conservative_roots.rs:358` and `:901` — `var_os("CRATONVM_SHADOW_STACK")`
  should be `cratonvm_types::flags().jit.shadow_stack`.

Behaviour-preserving (`parse::present` == `var_os(..).is_some()`). Rationale: with `gc`
and `jit` both centralized, `vm` is the last place the gate can diverge, and it is the
crate that owns the root scan — the half that must agree with codegen.

### R2 — `gc/src/tlab.rs` (+ `vm/src/jit/helpers.rs`): JIT allocations bypass pressure accounting

Owner: the `gc` sibling. `Tlab::alloc_initialized` bumps
`pressure.allocations_since_last_refill`, `pressure.alloc_count` and
`pressure.large_alloc_count`. The JIT inline TLAB bump updates **none** of them, and
neither does `jit_post_tlab_init`; the `skip_post_init_helper` fast path calls no helper at
all. With `inline_tlab_new_enabled()` default-ON, allocation-heavy JIT'd code is nearly
invisible to the pressure tracker.

Two possible resolutions, both cheap:
1. Emit the counter bumps inline (three adds against `[r10 + pressure_off]`, requires
   publishing the offsets through `JitRuntimeHelpers` like `tlab_cursor_offset_in_thread`).
2. Accept the gap explicitly and document that pressure is a *lower bound* — then audit
   every consumer of `pressure` for a decision that a systematic under-count would skew
   (refill sizing, GC trigger heuristics).

Please decide which; do not leave it undocumented. If (1), the offsets must go through the
helpers table, not be baked from a Rust struct — that is the pattern the cursor/end
offsets already follow and the reason that half of the contract is currently sound.

### R3 — `types/src/heap_types.rs` and `vm/src/jit/helpers.rs`: stale offset references

Owners: the `types` sibling and the `vm` sibling.

- `types/src/heap_types.rs:20-22` — the `HEADER_SIZE <= 127` assert's comment says "Bump
  to disp32 emission in `jit/src/x64.rs:6690-6813`". That range no longer contains any
  emitter (it is loop/BCE analysis on `6495a191c`). Suggested replacement text: *"the JIT
  emits array element offsets as signed disp8; see the site inventory in
  `docs/internal/arch-2026-07-26/x64-flag-skew-and-contracts.md` §6.2."*
- `types/src/heap_types.rs` — please add `pub const IDENTITY_HASH_CODE_OFFSET: usize = 8;`
  with the usual `offset_of!` assert. It is the only header field without a named
  constant, and it has at least two bake-in sites across the tree. `x64.rs` currently
  derives its own via `offset_of!` to avoid a literal; that local const should become a
  re-export once `types` has one.
- `vm/src/jit/helpers.rs:2247-2257` — the "Layout reminder" comment block in
  `jit_post_tlab_init` still describes the **old 40-byte** header (`off 20: gc_age +
  gc_flags`, `off 24: forwarding_ptr`, `off 32: mark_word`). The code below it is correct
  (it uses `GC_FLAGS_OFFSET` / `NUM_SLOTS_OFFSET`), so this is comment-only staleness — but
  it is exactly the comment someone will trust during the 32→16 shrink.
- `vm/src/jit/helpers.rs` — `*(raw_ptr.add(4) as *mut u32) = 0;` and
  `*(raw_ptr.add(8) as *mut i32) = hash;` are bare-literal header offsets in the Rust
  post-init path, mirroring the two the JIT side just had removed. They belong in the §6.2
  inventory but are outside this session's file ownership.

### R4 — `types/src/flags.rs`: the `JitFlags` expansion

Owner: whoever lands the `jit` flag migration. §3.3 above is a ready-made field list: 26
distinct semantics-affecting variables, with proposed names, defaults and the exact
parse helper each needs. Two specific asks:

- `CRATONVM_JIT_SAFEPOINT_REG_SPILL` must become one enum
  (`Off | CalleeSaved | NoStore | All`), not three booleans that each re-parse the same
  string against a different literal.
- The seven `CRATONVM_SHADOW_*` bisection toggles should be one bitflags field or be
  retired with the shadow-stack scaffolding.

---

## 8. Verification status

- Not built and not tested — this host runs nine concurrent agents and nine cargo builds
  OOM it. `rustfmt --check` was run: `jit/src/x64.rs` has **15** pre-existing formatting
  deviations before this session's edits and **the same 15** after (same sites, shifted by
  the inserted lines). No new deviation was introduced.
- CRLF line endings verified preserved (`file` reports CRLF before and after every edit;
  `git diff --stat` shows a small localized diff, not a whole-file rewrite).
- The `#[cfg(test)]` coverage added in `mod flag_and_header_contracts` is unrun. It is
  written to be environment-independent (no `set_var`, no reliance on a particular flag
  value) so it is safe to run in any harness.
