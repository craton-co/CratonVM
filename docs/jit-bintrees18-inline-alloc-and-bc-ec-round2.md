# Round 2 JIT codegen: bintrees18 inline-alloc header corruption + BC-EC ban

Scope: edits confined to `jit/src/x64.rs` and `vm/src/jit/skip_list.rs`
(plus this note). No interpreter / vm_exec / native-builtins changes.

## BUG 1 — bintrees18 inline-allocation header corruption — **FIXED**

### Symptom

```
./target/release/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" \
  --stack-dump-on-timeout 0 --Xmx 8g -cp bench BenchSuite bintrees18
```

Repeating GC warning, then derail / `rc=124` timeout:

```
GC: inconsistent header — kind=Object but array_length=1
  (num_slots=384, class_id=4); inline-alloc forgot to set kind=Array.
  Treating as corrupt so the walker can re-sync.
```

### Triage (existing release binary, no rebuild)

| Toggle                                   | Result            |
|------------------------------------------|-------------------|
| default (JIT on, inline-new on)          | 2 corruptions, rc=124 |
| `CRATONVM_DISABLE_JIT=1`                  | **0** corruptions |
| `CRATONVM_JIT_DISABLE_INLINE_NEW=1`      | **0** corruptions |
| `CRATONVM_DISABLE_SCALAR_REPLACEMENT=1`  | 2 corruptions (not SR) |
| `bintrees16 -Xmx8g` / `bintrees14 -Xmx512m` | 0 (working set < young) |
| `bintrees18 -Xmx2g`                      | 2 (more young GC) |

So the corruption is **specific to the inline-TLAB `new` fast path**
(`emit_inline_tlab_new`) and is **GC-timing dependent** — it only appears
once the live tree exceeds young-gen capacity and minor GC / the
non-moving sweep starts running.

### Root cause

`binaryTrees` allocates only `BenchSuite$Node` objects (`{Node l, r;}`,
2 reference fields → `num_slots=2`, total size `40 + 2*16 = 72`). It never
allocates an array, so the "kind=Array" text in the warning is the GC
walker's *guess*: it is really a **misread of a `Value` field cell as an
object header**.

Decode of the misread header against the 40-byte `#[repr(C)] ObjectHeader`
layout (`types/src/heap_types.rs`) and the 16-byte `Value` field-cell
layout (discriminant word at offset 0; `Object` discriminant = **4**;
8-byte object-pointer payload at cell offset 8):

* `class_id = 4`  ← the cell's discriminant word (`Value::Object`)
* `array_length = 1` ← header offset 12 = cell bytes 12..16 = the **upper
  32 bits of the 8-byte heap pointer** (a young-gen pointer near the 4–8 GB
  range under `-Xmx 8g` has upper-dword `1`)
* `num_slots = 384` ← header offset 16 = the *next* cell's discriminant /
  payload region

i.e. the linear heap walker **stepped 40 bytes (exactly `HEADER_SIZE`) into
a Node's own field region** and decoded field-0's `Value::Object(Some(ptr))`
cell as a header. That only happens if the *previous* object was sized as
`HEADER_SIZE + 0*SLOT_SIZE = 40` — i.e. its `num_slots` read as **0**.

`num_slots == 0` for a live `Node` means the walker observed the object
while its header was still the **TLAB-zeroed pattern** (`class_id=0,
kind=Object, num_slots=0`). The old `emit_inline_tlab_new` ordering was:

```
  MOV [thread.tlab.cursor], RAX     ; (1) COMMIT — object is now published
  MOV [obj+0],  class_id            ; (2) header writes happen AFTER
  MOV [obj+4],  0
  MOV [obj+12], 0
  MOV [obj+16], num_slots
```

Between (1) and (2) the object's region is already inside the "used"
portion of the TLAB / young arena (any heap walk that reaches it strides by
its header), but the header is still zeroed. The prior "fix" for the same
BinTrees-18 symptom only moved the `num_slots` write *inline* (out of the
post-init helper) — it did not move it **before the commit**, so the
publish-before-initialize window remained and the corruption recurred.

### Fix (`jit/src/x64.rs`, `emit_inline_tlab_new`)

Reordered so the **entire header is written before the TLAB-cursor commit**:

```
  MOV [obj+0],  class_id            ; header FIRST
  MOV [obj+4],  0                   ; kind=Object / elem=Ref / pad
  MOV [obj+12], 0                   ; array_length = 0
  MOV [obj+16], num_slots           ; walker stride
  MOV [thread.tlab.cursor], RAX     ; commit LAST — single publish point
```

The cursor store is the single linearization point that makes the object
reachable to a heap walk; on x86-64's TSO memory model a store is never
reordered ahead of older stores, so no walker (non-moving young sweep under
`gc_quiescence`, Cheney to-space scan, or a background-thread STW that parks
this mutator at a poll inside `jit_post_tlab_init`) can ever observe a
committed-but-unheadered object. This mirrors the interpreter / slow-path
allocators, which write the full `ObjectHeader` *before* the allocation is
visible.

`identity_hash_code` (offset 8) and reference field cells stay TLAB-zeroed;
the `jit_post_tlab_init` helper still mints the hash and applies
primitive-typed defaults (reference cells remain `null`-equivalent, which is
walk-safe — a zeroed cell decodes as `Value::Int(0)`, never followed as an
oop by the Cheney scan).

Confidence: high that this closes the documented publish-before-initialize
window that produces the exact observed misread. Verification requires the
orchestrator's build + `bintrees18` re-run.

## BUG 2 — BC-EC alloc/dispatch miscompile + the `org/bouncycastle/` ban — **NOT lifted (intentionally)**

### Repro (ban lifted via env)

```
cd apps/_test-suites/bc-java
CRATONVM_JIT_ALLOW_PACKAGES='org/bouncycastle/' \
  ../../../target/release/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" \
  -Xmx1g -cp "<core main;test;resources>;$TEMP/junit-3.8.2.jar" \
  junit.textui.TestRunner org.bouncycastle.math.ec.test.AllTests
```

Deterministic `EXCEPTION_ACCESS_VIOLATION` at `pc=…0029`,
`Faulting access: read at address 0x31`, `rax=1 rdx=1`.

### Findings (round 2)

* `0x31 = 1 + 0x30`, and `0x30 = HEADER_SIZE(0x28) + FIELD_CELL_PAYLOAD64_OFFSET(8)`:
  an **inline `getfield` on field 0** is dereferencing an object whose base
  is the integer **`1`**.
* `CRATONVM_JIT_DISABLE_INLINE_NEW=1` → **crash unchanged** (same pc, same
  `rax=1/rdx=1`). So BUG 2 is **independent of inline allocation** and of
  the BUG 1 fix — they do **not** share a root cause.
* `DISABLE_INLINE_GETFIELD=1` → the fault **moves** to `read at 0x0F`
  (`= 3 + 0xC`, an `arraylength` read with base `3`) and persists. The
  dereference site is only a witness; the bad primitive value (`1`/`3`) is
  produced **upstream** and lands in a slot that a later inline
  `getfield`/`arraylength` consumes as an object/array base. This is the
  exact "primitive-in-receiver-slot" signature in
  `docs/bc-math-ec-jit-miscompile-investigation.md`.

The round-1 fixes (dup/swap oop-mark sync, escape-analysis per-block
provenance barrier, dup2 cat-2 guard) are sound but do not cover this
producer. Single-package bisection does not reproduce it in isolation — it
needs many BC packages JIT-compiled together (a cross-package JIT→JIT
dispatch / operand-slot-reuse interaction). Pinning the exact producing
basic block requires an iterative build+bisect loop, which is out of scope
for an observe-only round.

### Decision

The `org/bouncycastle/` blanket ban in `vm/src/jit/skip_list.rs` (~line 457)
is **kept**, with an updated comment recording the round-2 findings. Lifting
it would re-introduce a hard `rc=139` SIGSEGV on every EC client. The ban is
revertible in one place (still gated by
`CRATONVM_JIT_ALLOW_PACKAGES=org/bouncycastle/`) so the orchestrator can
flip it once the upstream producer is fixed.

## Files touched

* `jit/src/x64.rs` — `emit_inline_tlab_new`: header-before-commit reorder
  (+ updated layout doc-comment).
* `vm/src/jit/skip_list.rs` — updated the `org/bouncycastle/` ban comment
  (ban itself unchanged).
* `docs/jit-bintrees18-inline-alloc-and-bc-ec-round2.md` — this note.
