# Bug Z — `TestFileStoreConcurrency` SIGSEGV (near-null read in the GC region)

**Severity:** High (hard VM crash). Class:
`org.apache.catalina.session.TestFileStoreConcurrency`.
**Status on CratonVM:** **FIXED** (branch `fix/tomcat-hardcrashes`, commit
`31d4f989`) — was a deterministic CRASH. **HotSpot:** FAIL (an assertion /
expected-exception mismatch — *not* a crash, so this is a genuine CratonVM-only
defect, not env-shared behavior).
**Run:** full Tomcat suite `tc1` (dev `bfed13f5`, the only genuine CRASH in 651
classes). Re-verified standalone 3×: crashes every run at the **same faulting
RVA**, so it is deterministic, not a parallel-contention artifact.

## Symptom

```
# EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005) at pc=…+0x1DF8E9
#  Faulting access: read at address 0x0000000000000014
#  thread: "main-vm"
Registers: rax=0x80  rcx=0x10  rdx=0x1BD57818  rbx=0x820
```

Preceded by:
```
WARN [org.apache.catalina.session.FileStore] Invalid persistence file
  […\STORE_TMP\1.session] for session ID [1]
```

The fault is a **read at `0x14`** (offset 20 from a near-null base — `rcx=0x10`),
i.e. a header/field read through an almost-null pointer. The faulting RVA
`0x1DF8E9` sits in the **`cratonvm_gc::gen_heap`** region (cf. the Bug-D crash
dump, where `gen_object_total_size` symbolized to `…+0x1DCD29`), so this looks
like a **GC walk dereferencing a bad/near-null object header**.

## Context

`TestFileStoreConcurrency` spins up **three threads** (save / load / remove) that
hammer a single `FileStore` for a fixed run time, then `join`s them. So it is
heavy concurrent allocation (session objects + serialization) across three
mutators with frequent GC — the same shape (multi-thread + GC) as the
already-fixed crash family in this suite (BUG-V monitor desync; the crash-#2
stale-TLAB / stale-oop-across-`grow()` cascade). The "Invalid persistence file"
warning means the load thread read a session file mid-write/mid-remove (a
save/remove race), so a native on the deserialize/load path may be handing back a
near-null object that a subsequent access (or the next GC walk) dereferences.

## Reproduce

```
cratonvm.exe -cp <tomcat-test-cp> org.junit.runner.JUnitCore \
  org.apache.catalina.session.TestFileStoreConcurrency
# SIGSEGV read@0x14, RVA 0x1DF8E9; HotSpot: FAIL (no crash).
```

## Root cause (symbolized) — moving GC reads a garbage `pointer_map` value

`release-with-debug` symbolized chain (read at a **garbage near-null pointer**,
which varies run to run — `0x14`, `0x1264`, … — confirming a bad *pointer*, not a
fixed offset):

```
gen_heap::gen_object_total_size           gen_heap.rs:5294   ← reads header at garbage new_addr
gen_heap::collect_garbage_inner +0x25F2   gen_heap.rs:2704   ← `for new_addr in pointer_map.values()` post-copy stats loop
interpreter::maybe_gc_forced              interpreter.rs:506 ← MOVING young GC
exceptions::create_exception_object       exceptions.rs:108  ← allocating the exception triggers the GC
exceptions::throw_runtime_error           exceptions.rs:481
execute_frame / execute / invoke_*        (the load thread throwing on the bad file)
```

So: the load thread reads a half-written/removed session file → throws → the VM
allocates the exception (`create_exception_object`) → that allocation forces a
**moving** young GC → the GC's post-copy loop dereferences a **garbage value in
`pointer_map`** (`new_addr ≈ 0x10`, a small integer, not a heap pointer) → SIGSEGV.
This is the Bug-D *victim* loop again (the moving collector walking a corrupt
heap), but the corruption source is the multi-threaded `save`/`load`/`remove`
churn, **independent of the JIT** (`CRATONVM_DISABLE_JIT=1` still crashes).

## Fix (attempt)

`maybe_gc_forced` ([interpreter.rs:477]) — the GC initiator path that fires from
allocation failure / `create_exception_object` — was the **only** initiator that
did **not retire its TLAB** before collecting (`maybe_gc` and
`force_gc_from_native` both do). An un-retired initiator TLAB leaves `[cursor,end)`
in young-from across the moving collection; after the young swap+reset the stale
TLAB hands out memory the collector considers free — the same use-after-free /
heap-desync class as the parked/blocked-thread TLAB bugs fixed in BUG-W. Fix:
`thread.tlab.retire()` at the top of `maybe_gc_forced` (matching its siblings).
This is a genuine inconsistency bug and is **kept** (bt18 = 68332206, no
regression), but it **does NOT resolve this crash** — FileStore still SIGSEGVs
3/3 with it applied. So the corrupt `pointer_map` value is not the un-retired
initiator TLAB.

## FIXED — reject non-heap forwarding addresses at the source

A guard added to the post-copy stats loop (`gen_heap.rs:2694`) that skips +
logs (instead of dereferencing) a non-heap `new_addr` confirmed the source: every
bad entry had a **valid `young_from` `old_addr`** but a **garbage `new_addr`** —
`0x10`, `0x300000000`, `0x11e0`, and several `old − 0x18/0x38/0x58`. So a live
`young_from` object's **`forwarding_ptr` header field holds 8-aligned non-null
garbage**, and `forward_object`'s existing sanity check (`gen_heap.rs:4573`) only
rejected `null` / non-8-aligned forwards — the garbage passed and was recorded
into `pointer_map`, sending the post-copy walk (and `update_all_roots`) into a
wild pointer.

**Fix (`31d4f989`):** a real forwarding address must land inside `young_to` or
`old_gen`; `forward_object` now rejects anything else (returns `old_ptr` unmoved,
exactly as the null/unaligned arm) so the garbage never enters `pointer_map`.
Plus the post-copy stats loop validates each value is in-heap before deref and
logs one summary line per cycle. **Verified:** FileStore CRASH→clean ×3 (no
SIGSEGV; the stats guard never fires — the source fix catches it first),
`bt18=68332206 / bt16=14985902 / bt14=3222190` (no GC regression).

## Follow-up — why the `forwarding_ptr` holds garbage (now harmless)

The garbage `pointer_map` *value* (a small integer like `0x10`/`0x1264`) is
inserted by `forward_object` during the multi-threaded moving collection under
the 3-thread `save`/`load`/`remove` churn, and is **independent of the JIT**
(`DISABLE_JIT` still crashes) and of the initiator-TLAB retire. Candidate
sources to chase next (Bug-D-class GC investigation):
- a **stale entry in a deposited root snapshot** (`collect_all_root_snapshots`):
  a thread parked/blocked with a snapshot ref that died or was a non-object
  `Value` misread as an `ObjectRef`, so `forward_object` produces a bogus
  forward;
- `forward_object`'s young-to / old-gen allocation returning a bad pointer near
  to-space exhaustion under the concurrent churn;
- a data race on the shared `pointer_map` / to-space cursor across the 3 mutators
  even under STW (e.g. a thread not actually parked when `wait_for_all` returns).
Decisive next tool: a guard in the post-copy loop (`gen_heap.rs:2694`) that, on a
non-heap `new_addr`, logs the offending `old_addr→new_addr` entry instead of
dereferencing — to identify which `forward_object` call site produced it — then a
heap-write / pointer_map-insert watchpoint. This is the only genuine crash left
in the 651-class suite (see `SUITE-RESULTS-tc1.md`).
