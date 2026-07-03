# Tomcat DoHead — main-vm mark-phase boundary read (SIGSEGV) — ROOT-CAUSED + CLAMPED

Status: **crash face FIXED** (branch `fix/dohead-sweep-freelist` commit
`8e64d9a5`, 2026-07-03); upstream corrupt-header producer still open (see
Residual). Originally filed as a suspected "JIT-called array helper" fault;
byte-exact disassembly of the crashed binary refuted that.

## Root cause (disassembly-attributed, arithmetic exact)

The faulting pc (exe RVA `0x20214C`, `mov r15,[rdi+r14*8+0x28]`) is the
reference-array arm of `for_each_ref_slot` inlined into the **young-mark BFS**
of `sweep_young_non_moving` (gc/src/gen_heap.rs), reached from the JIT
allocation slow path (alloc helper → collect_garbage → mark) — hence
"main-vm", the JIT frames below, sub-test-startup timing, and ZERO GC warnings
(the run died mid-mark; every garbage element read before the fault was
silently absorbed by `mark_young`'s `in_young` rejection).

A conservative-root/BFS candidate at `rdi=0x3AE0D9B0` had packed-pointer DATA
where a header should be: it misparsed as `kind=Array, element_type=Reference,
array_length=0x3AE0D928` — the low 32 bits of a heap address 136 bytes below
the object (rbx in the dump). `mark_young`'s gate caps `array_length` only at
`i32::MAX` (987M passes) and never cross-checks the claimed EXTENT against the
arena; the scan then marched `(0x3E960000-0x3AE0D9B0-0x28)/8 = 0x76A4C5`
elements (~59.3 MB) and crossed the mapped-region boundary at exactly
`0x3E960000`. (The original doc's "r13=1115 is the real bound" guess was wrong
— r13 was the mark-worklist length.)

## Fix (8e64d9a5)

A real object's extent always fits inside the generation it was allocated
from, so an out-of-extent header is definitionally corrupt. `mark_young`
(young BFS gate) and `scan_object_for_old_refs` (old BFS) now compute
`gen_object_total_size` and reject candidates whose extent leaves the
generation — no mark, no scan (retention-safe). Rate-limited diagnostics dump
the surrounding words + counter `SWEEP_BAD_EXTENT_HITS`, so any recurrence
attributes the upstream producer face directly.

## Residual (open)

The WRITER of the corrupt bytes is unconfirmed. Most probable (synthesis of
3-reader analysis): stale packed-pointer content in a reclaimed/reused young
slot — the accepted Layer-1 register-invisibility residual — or the
`try_alloc_young` publish→unlock→zero→late-header window (gen_heap.rs
~5824-5832 / ~1170-1183). The extent clamp converts the whole face into a
skipped root + log line regardless of producer. If `SWEEP_BAD_EXTENT_HITS`
warnings appear in future runs, their hex context distinguishes the faces.

Related finding (SEPARATE, still live on dev): the `7e2f2f9d` provenance
check's false-POSITIVE direction — never-evicted membership means a primitive
`long` whose bits equal a once-recorded (possibly freed) address passes
`object_ref_payload_is_known`, and the GC long-smuggle remap
(vm/src/runtime/value_stack.rs ~1418-1450) then REWRITES the long's bits via
the pointer map (silent value corruption; matches the observed wrong bt18
checksum 68332204). The `10a1be37` bitmap fix addressed only the perf convoy
and WIDENED false-positive acceptance to 64-byte granules. Needs its own fix
(eviction on free/move, or exact side-structure, or opt-out of long remap —
`longroot_strict`).

Crash artifacts: `apps/tomcat/.suite/results/dhswcomb-7/real-jit/*.log.err`.
