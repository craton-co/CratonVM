# The layout epoch guard cost three instructions because its counter was in the wrong place

**2026-09-10.** `emit_layout_epoch_guard` is emitted once per inline field
access site and executed on every one of them. It was three instructions and
nineteen bytes; it is now **one instruction and ten**. On a loop with four
guarded sites that is **14.5% faster** against a 2.5% noise floor. On a loop
with one site it is unmeasurable — which is the shape of the result, not a
caveat on it.

Follow-up to
[`c2-the-gp-register-file-is-not-the-binding-constraint-20260910.md`](c2-the-gp-register-file-is-not-the-binding-constraint-20260910.md),
whose §8 recorded this as a lead and sketched three routes. This is route 1.
Routes 2 and 3 are refused below, and the reason is worth more than the
measurement.

## What the guard is, and what it cost

A compact field access bakes `HEADER_SIZE + packed_body_offset` as an
immediate — a compile-time claim about a layout the class manager can replace
at run time. `LAYOUT_REPLACE_EPOCH` is the process-wide counter that only a
REPLACEMENT bumps, and the guard is what makes the baked offset safe:

```text
1e7: mov  r11, 7FF6BDFB9AF4h   ; &LAYOUT_REPLACE_EPOCH   -- 10 bytes of imm64
1f1: mov  ecx, [r11]
1f4: cmp  ecx, 0
1fa: jne  -> the jit_getfield helper
```

Three instructions and 19 bytes before the branch, burning R11 and RCX, to
read one `u32`. `probes/FieldLoop.java`'s loop body is about 27 instructions
and carries one of these; `probes/MultiFieldLoop.java` — added by this change,
four DISTINCT fields so GVN cannot collapse them — carries four.

## The encoding that should have been there

`CMP dword [rip+disp32], imm32` is `81 3D <disp32> <imm32>`: **one
instruction, ten bytes, no register clobbered**. The same shape
`emit_test_safepoint_flag_rip` already uses for the cooperative poll two
hundred lines below.

Emitting it changed nothing. Both arms kept the long form, because the
displacement did not fit — and the reason is the interesting part.

## The counter was in the executable image, and the code is not

Measured on this box:

| | address |
|---|---|
| `LAYOUT_REPLACE_EPOCH`, a `static` in `.data` | `0x7FF6BDFB9AF4` |
| the optimizing tier's code buffer | `0x1B430060000` |

About **140TB apart**. `disp32` reaches ±2GB. The short form was not merely
unused, it was **unreachable by construction**, on every compile, forever.

The safepoint poll gets away with it because its flag is not a static:
`stw_requested_flag_addr()` points into `shared.mem.gc_barrier`, a VM heap
allocation — and the poll's flag and the code buffer share a prefix
(`0x1E598CCA2C0` against `0x1E5892C0000`). Same heap, same region, in reach.

So the fix is not in the emitter at all. `LAYOUT_REPLACE_EPOCH` is now a
leaked `Box<AtomicU32>` behind a `LazyLock` — allocated from the same heap the
VM's own structures come from, addressed the same way, stable for the life of
the process because JIT code bakes it. One `static` becomes one heap cell and
the encoder's range check starts passing:

```text
1e7: 813d37247c2600000000   cmp dword [rel 20228042628h], 0
264: 813dba237c2600000000   cmp dword [rel 20228042628h], 0
```

Two sites at different program counters resolving to **one** address is also
the check that the displacement arithmetic is right: an off-by-one in the
instruction length would have given them two different targets.

This is best-effort and says so in the code. Nothing promises an allocator puts
two allocations within 2GB, so the range check and the materialize-the-address
fallback both stay. Moving the counter makes the short form **reachable**, not
certain — and `CRATONVM_JIT_IR_EPOCH_GUARD_RIP=0` exercises the fallback
deliberately rather than waiting for an address space that produces it.

## Engagement

`MultiFieldLoop.sumGuarded`, `full/ir` body, one binary:

| arm | `81 3D` guards | `MOV ECX,[R11]` guards | body bytes |
|---|---:|---:|---:|
| `EPOCH_GUARD_RIP=1` | **4** | 0 | **1815** |
| `EPOCH_GUARD_RIP=0` | 0 | 4 | 1851 |

36 bytes = 4 sites x 9. Exactly the arithmetic, and no site left behind.

## The measurement, and why the two shapes disagreeing is the point

`tools/tier-ab/flag-ab.sh`, one binary, `CRATONVM_JIT_FORCE_C2=1` in both arms,
14 rounds, arms interleaved with a control:

| probe | guard sites | A (off) | C (control) | B (on) | floor | effect |
|---|---:|---:|---:|---:|---:|---:|
| `FieldLoop` | 1 | 757 ms | 744 ms | 755 ms | 1.7% | +0.6% — **UNMEASURABLE** |
| `MultiFieldLoop` | 4 | 896 ms | 874 ms | **757 ms** | 2.5% | **-14.5%** |

Checksums identical in every run and equal to Temurin JDK 25's
(`FieldLoop acc=1500150000`, `MultiFieldLoop acc=1561300000`).

**The effect scales with the number of guard sites, which is what a per-site
cost has to do.** One site is two instructions out of ~27 in a loop the earlier
investigation showed to be latency-bound, and it disappears into the floor.
Four sites is eight instructions and it does not. A result that read 14.5% on
BOTH shapes would have been evidence the lever was not the guard.

### One reading is retracted here

An earlier pass at `FieldLoop` read **+6.0% SLOWER against a 5.9% floor** and
was on its way into this file as a contradiction. It was taken while three
other CratonVM benchmark sessions shared the box (`target-hibreactive-c2ab`,
`nettylocal`, `bytebuf`; the machine measured 76% CPU across 32 cores). Re-run
at a 1.7% floor the same lever reads +0.6%. The first number was describing the
machine, exactly as `tools/tier-ab/README.md` warns a floor above ~3% does, and
it is recorded here because a discarded measurement that is not written down
gets re-taken.

### What is NOT isolated

The heap move and the RIP encoding shipped together, and only the ENCODING is
A/B-able within one binary — both arms carry the heap counter. So "14.5%" is
the encoding's, measured cleanly; the heap move's own cost is **unmeasured**.
It is one global counter whose location changed, read by the same instruction
count in the off arm, so there is no mechanism for it to matter — but that is
an argument, not a number, and the two are not the same thing.

## Routes 2 and 3 are refused, and this is the part worth keeping

The lead sketched two larger routes. Both are unsound, for one reason.

**Route 2, hoist the guard to the loop preheader.** **Route 3, replace the
check with a dependency** — register the artifact against the epoch and let
`jit_cache.remove` / `invalidate_matching` drop it on a bump, the way HotSpot
handles an assumption.

Both fail on **in-flight frames**. Cache invalidation governs future ENTRIES to
a method, not a frame already executing one — the same fact that retired
artifact displacement in `docs/JIT_OPTIMIZATION.md`. A thread inside a loop
that has already passed a hoisted guard, or inside a method whose artifact was
just evicted, keeps reading at the baked offset. The per-access guard is
load-bearing precisely because it re-asks inside the loop.

Route 3 has a second, independent blocker: `register_class_layout` lives in
`types`, and `types` cannot call the JIT cache — the dependency runs the other
way.

What the guard does NOT promise is worth stating too, because it looks stronger
than it is: it narrows the check-to-use window, it does not close it. The bump
happens before the new layout is published, under the write lock, so a guard
that sees the old count raced an old layout that was still correct — but
nothing orders a thread's LOAD against a publish that happens after its check.
That is the existing design's own tolerance, and it is the reason widening the
window by hoisting is a change in kind rather than in degree.

## Reproducing

```bash
cargo build --release -p cratonvm-cli
javac -d /tmp/pc probes/MultiFieldLoop.java probes/FieldLoop.java

# engagement — 4 and 0, then 0 and 4
for r in 1 0; do
  CRATONVM_JIT_IR_EPOCH_GUARD_RIP=$r CRATONVM_JIT_FORCE_C2=1 \
  CRATONVM_DBG=jit-disasm CRATONVM_DBG_JIT_DISASM=MultiFieldLoop.sumGuarded \
    ./target/release/cratonvm -cp /tmp/pc -Dprobe.reps=3000 MultiFieldLoop 2>&1 \
    | awk '/full\/ir MultiFieldLoop/{n++} n==1' | grep -c 813d
done

# the number
bash tools/tier-ab/flag-ab.sh -Exe "$PWD/target/release/cratonvm" \
    -Cp /tmp/pc -Class MultiFieldLoop -Flag CRATONVM_JIT_IR_EPOCH_GUARD_RIP \
    -Base "CRATONVM_JIT_FORCE_C2=1" -Rounds 14 -D probe.reps=8000
```

Check the box is quiet first. Both numbers above came from runs whose control
pair agreed to under 3%; the ones that did not are retracted in this file
rather than in a later one.
