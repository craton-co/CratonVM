# One corrupt `Value` cell: reported on Generational, SIGSEGV on ZGC and G1

**Status: the crash is FIXED (guard parity). The producer that corrupts the
cell is OPEN and is NOT collector-specific — Generational reaches it too, and
its green is only green because it screens the read.**

Found re-running the 104 non-passing Spring Boot classes on a Windows host in
three shards, one per collector.
`configuration-metadata/spring-boot-configuration-processor` →
`JsonMarshallerTests` CRASHed on G1 and ZGC and PASSed 17/17 on Generational —
the same class/collector split an earlier Azure run had flagged and left
uninvestigated. Reproducing on a different host and OS ruled out anything
environmental.

## Measured

One binary, one class, `--Xmx 2g`, real JDK 25:

| arm | result |
| --- | --- |
| ZGC | SIGSEGV, 2/2 runs |
| G1 | SIGSEGV, 2/2 runs |
| Generational | `tests=17 failed=0`, 2/2 runs |
| ZGC/G1 `--nojit` | SIGSEGV, 2/2 runs each |
| ZGC/G1 `CRATONVM_COMPACT_REF_FIELDS=0` | SIGSEGV |

`--nojit` still crashes, so the crash header's
`unregistered-jit-frame-on-stack` line is incidental — that field reports the
last root-gathering pass, and with `jit: 0 compiled code range(s)` the crash is
unchanged. The compact-layout kill switch does not move it either, so the
packed-field offset path is not involved.

## The faulting instruction

```
cratonvm_gc::heap::coerce_field_value_for_slot+0x42
cratonvm_gc::vm_heap::VmHeap::get_field_as
cratonvm_vm::vm::vm_exec::impl$14::get_field
cratonvm_native_builtins::lang_string::decode_string_chars      (String.value)
cratonvm_native_builtins::lang_string::invoke_to_string_units_opt
cratonvm_native_builtins::lang_string::native_sb_append_object  (StringBuilder.append(Object))
```

Disassembling the function out of the PE image rather than trusting the line
table: `+0x39` is `mov eax, dword ptr [rdx]` — the `Value` discriminant — and
`+0x42`, the faulting instruction, is

```
49 63 04 82    movsxd rax, dword ptr [r10+rax*4]
```

a **jump-table load with no bounds check**, because Rust guarantees an in-range
discriminant. The register dump closes it arithmetically:

```
r10 = 0x00007FF7EEFFB914   (jump-table base)
rax = 0x00000000440A7D38   (the "discriminant")
r10 + rax*4               = 0x00007FF8FF29ADF4
Faulting access: read at address 0x00007FF8FF29ADF4     ← exact match
```

`rbx = r8 = 0x5B` = `'['`, so the arm being selected is the `b'L' | b'['` one —
the descriptor of `String.value`, a `[B`.

## Why one collector reported it and three died

`gen_heap::read_slot` has screened the discriminant since `HIB-CV-32`, and its
comment predicts this crash in as many words: decoding a corrupt cell and then
matching on it "is a wild jump-table SIGSEGV with no context". The other three
legacy-cell readers never got that screen:

| backend | site | read | screened |
| --- | --- | --- | --- |
| Generational | `gen_heap.rs::read_slot` | `read_value_checked_atomic` | **yes** |
| shared `Heap` | `heap.rs::read_slot` | `read_value_atomic` | no |
| G1 | `g1.rs::get_field` | `read_value_atomic` | no |
| ZGC | `zgc.rs::get_field` | `std::ptr::read` | no — and non-atomic |

Running the Generational arm and grepping for the guard settles that this is
one defect and not two:

```
gen_heap::read_slot: corrupt Value cell (out-of-range discriminant)
  slot=0x1863c73a098 raw0="0x000001863c50a5d8" raw1="0x000001863c7218e0"
```

**Exactly one hit, and then the class passes 17/17.** Both words are pointers
into the same live arena as the slot itself, i.e. the cell holds a two-reference
payload, not a `(tag, payload)` pair — a swept-then-reused object, which is
precisely the shape `HIB-CV-32` names. So the corruption happens under
Generational too. Its PASS is *masked*, not clean.

## Fixed here

All four readers now share one implementation,
`heap::read_value_cell_checked`, which screens the discriminant, returns a
benign null, and logs the same rate-limited `cratonvm::gc::guard` record. ZGC's
site additionally stops being a non-atomic `ptr::read` that could tear against a
concurrent plain writer.

This makes the *outcome* uniform. It does not make it correct.

## Confirmed on Linux too (2026-08-20)

Rebuilt on the Azure host (`vm1`, 8 cores, glibc) at `509710ba8` and re-run
against the fixture there, so the finding is not a Windows artefact:

| binary | ZGC | G1 | Generational |
| --- | --- | --- | --- |
| pre-fix (`/data/bin/cratonvm-gtc-base`, built before the fix landed) | `rc=139` SIGSEGV | — | PASS 17/17 |
| post-fix (`509710ba8`) | PASS 17/17 | PASS 17/17 | PASS 17/17 |

Linux reproduces the collector split exactly as Windows did — same class, same
crash, same collector passing.

The causal link is the guard's own site label. On the fixed binary it fires
**exactly once per run**, and the site names the collector's own reader:

```
ZGC           zgc::get_field: corrupt Value cell
G1            g1::get_field: corrupt Value cell
Generational  gen_heap::read_slot: corrupt Value cell
```

That is this page's claim in one line: three collectors reach the same corrupt
cell through three different readers, and before this change only the
`gen_heap` one screened it. It also confirms the Generational green was always
a screened read, not an absent defect.

The pre/post rows are different binaries — the screen has no kill switch, so a
same-binary A/B is not available here (see the cross-binary-A/B caveat). The
one-guard-hit-per-run evidence above is what ties the change to the outcome,
not the binary swap.

## Still open — the producer

Something hands `StringBuilder.append(Object)` a receiver whose memory has
already been swept and reused. `native_sb_append_object` roots `this` in a
`NativeHandleScope` — with a comment about exactly this hazard — but reads its
second argument straight out of the raw `args` slice and passes it unrooted to
`invoke_to_string_units`. That asymmetry is the obvious suspect and has not been
proven: the fault is on the `java/lang/String` fast path, which has no
allocation between entry and the read, so the reference is already stale when
the native is entered, which points upstream of this function rather than at it.

Next step is to find the producer, not to widen the guard. A run that is quiet
here is not a run that is correct — the guard reports only cells whose
discriminant lands out of range, and a stale reference whose recycled bytes
happen to form a valid `Value` is invisible to it.
