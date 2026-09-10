# The layout epoch guard cost three instructions because its counter was in the wrong place

**2026-09-10. Filed and closed the same day.** `emit_layout_epoch_guard` is
emitted once per inline field access site and executed on every one of them. It
was three instructions and nineteen bytes; it is now **one instruction and
ten**, in **both** tiers, on **both** platforms.

Getting to "both platforms" took a second pass, and the reason is the whole
value of this page: the first fix moved the counter off `.data` and onto the
Rust heap, checked it on Windows, and shipped. On Linux the guard went on
taking the long form — because what a RIP-relative operand needs is not "the
heap" but **the allocator the code cache uses**, and those are 123.9TB apart
here. §4 is that measurement.

Follow-up to
[`c2-the-gp-register-file-is-not-the-binding-constraint-20260910.md`](c2-the-gp-register-file-is-not-the-binding-constraint-20260910.md),
whose §8 recorded this as a lead and sketched three routes. This is route 1.
Routes 2 and 3 are refused in §9, and the reason is worth more than the
measurement.

## 1. What the guard is, and what it cost

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

## 2. The encoding that should have been there

`CMP dword [rip+disp32], imm32` is `81 3D <disp32> <imm32>`: **one
instruction, ten bytes, no register clobbered**. The same shape
`emit_test_safepoint_flag_rip` already uses for the cooperative poll two
hundred lines below.

Emitting it changed nothing. Both arms kept the long form, because the
displacement did not fit — and the reason is the interesting part.

## 3. The counter was in the executable image, and the code is not

Measured on the Windows box this was filed from:

| | address |
|---|---|
| `LAYOUT_REPLACE_EPOCH`, a `static` in `.data` | `0x7FF6BDFB9AF4` |
| the optimizing tier's code buffer | `0x1B430060000` |

About **140TB apart**. `disp32` reaches ±2GB. The short form was not merely
unused, it was **unreachable by construction**, on every compile, forever.

The safepoint poll appeared to get away with it because its flag is not a
static: `stw_requested_flag_addr()` points into `shared.mem.gc_barrier`, a VM
heap allocation — and on that box the poll's flag and the code buffer shared a
prefix (`0x1E598CCA2C0` against `0x1E5892C0000`). Same heap, same region, in
reach.

So the first fix made `LAYOUT_REPLACE_EPOCH` a leaked `Box<AtomicU32>` behind a
`LazyLock`: allocated from the same heap the VM's own structures come from,
stable for the life of the process because JIT code bakes it. The encoder's
range check started passing:

```text
1e7: 813d37247c2600000000   cmp dword [rel 20228042628h], 0
264: 813dba237c2600000000   cmp dword [rel 20228042628h], 0
```

Two sites at different program counters resolving to **one** address is also
the check that the displacement arithmetic is right: an off-by-one in the
instruction length would have given them two different targets.

## 4. The heap was the wrong place too, and Linux says so

The same binary, the same probe, on Linux:

```text
full/ir MultiFieldLoop.sumGuarded  entry=0x7DE4D7F9E000  len=1865
1ee: 49bb2004751e00020000   mov r11,2001E750420h     ; the leaked Box
1f8: 418b0b                 mov ecx,[r11]
1fb: 81f900000000           cmp ecx,0
```

**123.9TB apart**, and all four guards in that body took the long form —
exactly as they had when the counter was a `static`. The optimization was
inert on this platform, and nothing in the test suite could have said so: the
guard is correct at any address, only longer.

The premise was wrong in a way that is easy to repeat. "The heap is where the
VM's own structures come from" is true and beside the point. What a
RIP-relative operand needs is that the counter and the code come from the
**same allocator**:

| | allocator | where it lands |
|---|---|---|
| the JIT code cache | `mmap(NULL, …)` / `VirtualAlloc(NULL, …)` — `jit/src/platform.rs` | the process's mapping band, `0x7D…` here |
| a leaked `Box` | Rust's global allocator, which is mimalloc | mimalloc's own reserved arenas, `0x20…` here |

Being "on the heap" put the counter in the wrong band. On Windows the two
happened to coincide; on Linux they structurally do not.

So the counter is now **one OS page from the same primitive** — `mmap` /
`VirtualAlloc`, never unmapped, so the address is stable for the life of the
process the way JIT code baking it requires. A whole page for four bytes is
the point rather than waste: sharing a Rust allocation puts it back in the
wrong arena, and sharing a cache line with an unrelated hot word would
false-share against a counter every guarded field access reads. On the same
Linux box, same probe:

```text
full/ir MultiFieldLoop.sumGuarded  entry=0x7353D4916000  len=1829
1ee: 813d081e000000000000   cmp dword [rel 7353D4918000h],0
26b: 813d8b1d000000000000   cmp dword [rel 7353D4918000h],0
2dc: 813d1a1d000000000000   cmp dword [rel 7353D4918000h],0
34d: 813da91c000000000000   cmp dword [rel 7353D4918000h],0
```

The counter at `0x7353D4918000` now sits **between two code buffers**
(`0x7353D4914000` and `0x7353D4919000`), which is what "the same allocator"
buys. 1865 → 1829 bytes is 36 = 4 sites × 9, and the four sites again resolve
to one address.

This is still best-effort and says so in the code. Nothing promises two
mappings from one band land within 2GB, so the range check and the
materialize-the-address fallback both stay. Placing the counter here makes the
short form **reachable**, not certain — and
`CRATONVM_JIT_IR_EPOCH_GUARD_RIP=0` / `CRATONVM_JIT_SP_EPOCH_GUARD_RIP=0`
exercise the fallback deliberately rather than waiting for an address space
that produces it.

### What this leaves for someone else

`stw_requested_flag_addr()` is in the same wrong band on Linux — measured in
the same run, `0x2000CD6E2C0`, 123.9TB from the code cache — so the
2026-09-02 RIP-relative safepoint poll takes its `MOV R11, imm64` fallback on
every back edge of every compiled loop on this platform. That is the same root
cause and a different owner: the flag is a field inside `shared.mem.gc_barrier`
rather than a standalone cell, so moving it is a GC-side change and not this
one. Recorded here because it was measured here.

## 5. The single-pass tier had the same guard and not the same encoding

Route 1 landed in the optimizing tier only. The single-pass tier kept the long
form on the stated grounds that its native unroller **byte-copies** loop
bodies, so a displacement that is right at the original site names
`target + shift` from the copy.

That hazard is real and it already had a fixup. `rip_abs_disp32_patches`
re-resolves every RIP-relative absolute displacement per unrolled copy against
that copy's own PC, and carries a per-entry **trail** — the bytes emitted after
the displacement, which the CPU measures from. The safepoint poll rides it with
a trail of 1 for its `imm8`. The guard needs 4 for its `imm32`, and that is the
whole difference.

The trail is also the only thing here that fails silently. A guard whose trail
under-counts by 3 is correct in the ORIGINAL body and three bytes off in every
unrolled copy, so the first iteration answers about the epoch and the rest
answer about whatever is three bytes past it — which is why
`rip_relative_epoch_guard_addresses_the_counter_and_declares_its_trail` asserts
the registered trail and not just the encoding.

This tier unrolls, so the saving is per site **per copy**: `MultiFieldLoop`'s
four sites become eight guards in the `osr/sp` body that actually runs.

## 6. Two single-pass sites had no guard at all

`51aee440b` set out to guard "every baked compact cell offset" and its message
names five sites. The census missed two, both in the single-pass bytecode walk:

* the **inline compact `getfield`** (`0xb4`) — the hottest field path in the VM,
  and the one `FieldLoop` and `MultiFieldLoop` measure;
* the **ungated inline compact reference `putfield`**, the arm that runs when
  `emit_gated_compact_ref_putfield` declines for want of a published barrier
  plan.

Both baked `HEADER_SIZE + packed_body_offset` and both went on using it across
a `register_class_layout` replacement, which the allocation emitters' own
comment calls "confirmed heap corruption". This is not a performance residual
and it is recorded on a performance page only because that is where it was
found: chasing the guard's ENCODING is what made someone read every site that
emits one.

The putfield arm takes the guard as one more `bail` — every bail there already
means "take the compact-aware helper", which resolves the current layout. The
getfield arm needed more. RAW mode (`CRATONVM_JIT_INLINE_GETFIELD`, opt-in)
emits no helper tail at all, and **neither of its two existing exits is a
correct destination for a replaced layout**: the null path answers 0, and the
legacy path reads a compact object at the uniform slot offset. So raw mode
grows the same tail the guarded mode already has and jumps over it on the null
path — and flushes the scratch cache, because it now has a call on a path where
it never had one.

`every_single_pass_compact_field_site_guards_its_baked_offset` pins both, and
counts guards by RESOLVING to the counter's address so it recognises either
encoding — a test that knew only the short form would pass or fail by which
band the allocator picked that day. Writing it caught a second version of the
same mistake: the fallback's load is `41 8B 8B 00000000` here, not the
`41 8B 0B` an emitter that wanted the shortest encoding would produce, and a
matcher written from the manual rather than from the disassembly would have
recognised neither arm on a box where the counter is out of reach.

## 7. Engagement

`MultiFieldLoop.sumGuarded`, one binary, Linux. Guards are counted by
RESOLVING each candidate to the counter's address, because `813d` also occurs
inside unrelated `MOV RAX, imm64` operands and counting opcodes gets it wrong:

| tier, arm | short-form guards | long-form guards | body bytes |
|---|---:|---:|---:|
| `full/ir`, `IR_EPOCH_GUARD_RIP=1` | **4** | 0 | **1829** |
| `full/ir`, `IR_EPOCH_GUARD_RIP=0` | 0 | 4 | 1865 |
| `osr/sp`, `SP_EPOCH_GUARD_RIP=1` | **8** | 0 | **2423** |
| `osr/sp`, `SP_EPOCH_GUARD_RIP=0` | 0 | 8 | 2527 |
| `osr/sp`, `SP_FIELD_LAYOUT_GUARD=0` — §6's fix removed | 0 | 0 | 2287 |

Three separate pieces of arithmetic, and all three come out:

* **36 = 4 × 9** in the optimizing tier. Its fallback is `MOV R11, imm64` (10)
  + `MOV ECX, [R11]` (3) + `CMP ECX, imm32` (6) = 19, against 10.
* **104 = 8 × 13** in the single-pass tier, and the extra four bytes per site
  are real rather than a miscount: this backend's load goes through
  `Disp::encode_for_base`, which emits `41 8B 8B 00000000` — `mod=10` with an
  explicit zero disp32, 7 bytes — so its long form is 23 and not 19.
* **136 = 8 × 17** for the guard's own existence, of which 16 is the compare
  and its `JNE` and the last byte is displacements widening around them.

Eight guards for four sites is the unroll: the `osr/sp` body carries two
copies, each with its four, and each copy's displacement was re-resolved
against its own PC by `rip_abs_disp32_patches` — which is the fixup §5 is
about, working.

## 8. The measurement

`tools/tier-ab/flag-ab.sh`, one binary, arms interleaved with a control.

<!--MEASUREMENTS-->

## 9. Routes 2 and 3 are refused, and this is the part worth keeping

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

## 10. One reading is retracted here

An earlier pass at `FieldLoop` read **+6.0% SLOWER against a 5.9% floor** and
was on its way into this file as a contradiction. It was taken while three
other CratonVM benchmark sessions shared the box (`target-hibreactive-c2ab`,
`nettylocal`, `bytebuf`; the machine measured 76% CPU across 32 cores). Re-run
at a 1.7% floor the same lever reads +0.6%. The first number was describing the
machine, exactly as `tools/tier-ab/README.md` warns a floor above ~3% does, and
it is recorded here because a discarded measurement that is not written down
gets re-taken.

## 11. Reproducing

```bash
cargo build --release -p cratonvm-cli
javac -d /tmp/pc probes/MultiFieldLoop.java probes/FieldLoop.java \
      probes/RefFieldStoreLoop.java

# is the counter in reach on THIS box? The guard's own disassembly is the
# answer -- `cmp dword [rel ...]` is, `mov r11, imm64` is not.
CRATONVM_JIT_FORCE_C2=1 CRATONVM_DBG=jit-disasm \
CRATONVM_DBG_JIT_DISASM=MultiFieldLoop.sumGuarded \
  ./target/release/cratonvm -cp /tmp/pc -Dprobe.reps=3000 -Dprobe.n=200 \
  MultiFieldLoop 2>&1 | grep -E "entry=|epoch|cmp dword \[rel|mov ecx,\[r11\]"

# engagement -- 4 and 0, then 0 and 4
for r in 1 0; do
  CRATONVM_JIT_IR_EPOCH_GUARD_RIP=$r CRATONVM_JIT_FORCE_C2=1 \
  CRATONVM_DBG=jit-disasm CRATONVM_DBG_JIT_DISASM=MultiFieldLoop.sumGuarded \
    ./target/release/cratonvm -cp /tmp/pc -Dprobe.reps=3000 MultiFieldLoop 2>&1 \
    | awk '/full\/ir MultiFieldLoop/{n++} n==1' | grep -c 813d
done

# the encoding, optimizing tier
bash tools/tier-ab/flag-ab.sh -Exe "$PWD/target/release/cratonvm" \
    -Cp /tmp/pc -Class MultiFieldLoop -Flag CRATONVM_JIT_IR_EPOCH_GUARD_RIP \
    -Base "CRATONVM_JIT_FORCE_C2=1" -Rounds 14 -D probe.reps=8000

# the encoding, single-pass tier
bash tools/tier-ab/flag-ab.sh -Exe "$PWD/target/release/cratonvm" \
    -Cp /tmp/pc -Class MultiFieldLoop -Flag CRATONVM_JIT_SP_EPOCH_GUARD_RIP \
    -Base "CRATONVM_C2_SUPERSEDE=0" -Rounds 14 -D probe.reps=8000

# the counter's LOCATION, with the encoding held at the long form in both arms
bash tools/tier-ab/flag-ab.sh -Exe "$PWD/target/release/cratonvm" \
    -Cp /tmp/pc -Class MultiFieldLoop -Flag CRATONVM_JIT_LAYOUT_EPOCH_STATIC \
    -Base "CRATONVM_JIT_FORCE_C2=1,CRATONVM_JIT_IR_EPOCH_GUARD_RIP=0" \
    -Rounds 14 -D probe.reps=8000

# what §6's correctness fix costs
bash tools/tier-ab/flag-ab.sh -Exe "$PWD/target/release/cratonvm" \
    -Cp /tmp/pc -Class MultiFieldLoop -Flag CRATONVM_JIT_SP_FIELD_LAYOUT_GUARD \
    -Base "CRATONVM_C2_SUPERSEDE=0" -Rounds 14 -D probe.reps=8000
```

Check the box is quiet first. Every number above came from runs whose control
pair agreed to under 3%; the ones that did not are retracted in §10 rather than
in a later file.
