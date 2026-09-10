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

> **And on System V it was never certain, it was never true.** Checked on
> retirement, on an Ubuntu 24.04 x86-64 host, one process:
>
> | | address |
> |---|---|
> | `LAYOUT_REPLACE_EPOCH`, the leaked `Box` | `0x2001E8103F0` |
> | `stw_requested_flag`, the poll's cell | `0x2000CD6E2C0` |
> | the optimizing tier's code buffer | `0x7A53DBCB8000` |
>
> The two heap cells are 295 MB apart — comfortably in reach of each other —
> and the code is **130 TB** from both. So on Linux `MultiFieldLoop.sumGuarded`
> emits **zero** short-form guards with `EPOCH_GUARD_RIP=1` and zero with it
> off: byte-identical bodies, and the 14.5% below is a Windows number in its
> entirety. **The back-edge safepoint poll is in the same position** — its
> `TEST byte [rip+disp32]` also never encodes there, which retires this file's
> "the poll gets away with it because its flag is not a static" as an
> explanation that happens to be true only on Windows. The real rule is that
> `mmap(NULL, …)` places code in the kernel's own high region while mimalloc
> reserves its arenas near 2 TB, and nothing moves those towards each other.
>
> That is a placement problem, not an encoder one, and it now has a lever:
> `CRATONVM_JIT_CODE_NEAR_GLOBALS=1` gives `mmap` an address HINT derived from
> the epoch counter, so code buffers land inside the window when the address
> space allows. Hint only — no `MAP_FIXED`, so it can never overlap the heap,
> a reserved arena, or anything the GC reads, and a placement out of window is
> released and retried. See §"On Linux the counter was in the right place and
> the CODE was in the wrong one" below.

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

## On Linux the counter was in the right place and the CODE was in the wrong one

**Added 2026-09-10, on retirement.** Everything above was measured on Windows.
Checked on Linux, none of it engages, and the reason is worth as much as the
original finding.

`MultiFieldLoop.sumGuarded`, Ubuntu 24.04 x86-64, `full/ir` body, both arms of
the flag:

| arm | `81 3D` guards | body bytes |
|---|---:|---:|
| `EPOCH_GUARD_RIP=1` | **0** | 1865 |
| `EPOCH_GUARD_RIP=0` | **0** | 1865 |

Byte-identical bodies. The flag this file is about changes nothing on Linux,
and neither does the heap move that made it possible — because the code buffer
is not in the heap's part of the address space:

```text
LAYOUT_REPLACE_EPOCH (mimalloc heap)   0x2001E8103F0
stw_requested_flag   (mimalloc heap)   0x2000CD6E2C0    295 MB from the counter
optimizing tier's code buffer (mmap)   0x7A53DBCB8000   ~130 TB from both
```

So the back-edge safepoint poll takes the long form here too — `MOV R11,
imm64` then `TEST BYTE [R11], 0FFh` — which retires this file's explanation of
why the poll "gets away with it". It gets away with it **on Windows**, where
the VM heap and the JIT's code cache are both in the same low region. On Linux
`mmap(NULL, …)` places anonymous mappings in the kernel's high region and
mimalloc reserves its arena at 2TB, and nothing pulls those together.

### It is a placement problem, and placement is a hint away

`CRATONVM_JIT_CODE_NEAR_GLOBALS=1` (`jit::platform`, **default OFF**, Unix
only) supplies `mmap` with an address hint derived from the epoch counter, so
code buffers land inside the ±2GB window when the address space allows. A hint
and nothing more — no `MAP_FIXED` — so it cannot overlap the heap, a reserved
arena, or anything the GC reads; a placement that comes back out of window is
released and the next hint tried, and a whole ladder of misses retires the
strategy for the process.

The first version of that walk found nothing, and `/proc/<pid>/maps` says why —
it is the same kind of fact as the 140TB above and deserves the same billing:

```text
20000000000-20040000000 rw-p [anon:mimalloc]
```

**One 16GB reservation with the anchor 511MB inside it.** A cursor walking
*up* from the anchor is inside that mapping for the whole reachable window, so
every hint was relocated. The free space is *below* it — the arena is reserved
upward from its base, so the room is underneath. The hint ladder now probes
both sides with doubling offsets, below first, and
`the_ladder_probes_below_the_anchor_first` pins that as a property rather than
as a comment.

### It engages, and the arithmetic checks out

Same probe, same binary, `MultiFieldLoop.sumGuarded`:

| arm | code buffer | `81 3D` guards | `F6 05` polls | body bytes |
|---|---|---:|---:|---:|
| `CODE_NEAR_GLOBALS=0` | `0x768E40F56000` | 0 | 0 | 1865 |
| `CODE_NEAR_GLOBALS=1` | `0x1FFFEC00000` | **4** | **2** | **1815** |

The ON arm's buffer sits at `0x1FFFEC00000` — 20MB below mimalloc's arena base
and 511MB below the counter, which is the ladder's 512MB rung landing exactly
where `/proc/self/maps` said the room was. 50 bytes = 4 guards x 9 + 2 polls x
7, so every guard AND every back-edge poll in the body took the short form and
none was left behind.

That is the same engagement table as the Windows one further up, reached on the
platform where the original change did nothing at all.

### And it is worth 7% on the four-site loop

`flag-ab.sh`, one binary, `CRATONVM_JIT_FORCE_C2=1` in both arms,
`probe.reps=8000`, `MultiFieldLoop`, twice:

| run | host load | A | C | B | floor | effect |
|---|---:|---:|---:|---:|---:|---:|
| 12 rounds | 7 | 928.5 | 921.5 | **860.5** | 0.8% | **−7.0% — ON FASTER** |
| 14 rounds | 38 | 1131.0 | 1103.5 | **1077.5** | 2.5% | **−3.6% — ON FASTER** |

Checksums identical in every run (`acc=4161300000`). Two invocations, one on a
quiet host and one on a saturated one, agreeing on the sign — which after this
file's companion (`…gp-register-file…`, §5.2) is the bar a few-percent claim
has to clear, not a single clean floor.

Half the Windows figure for the same shape. That is roughly what it should be:
the Windows 14.5% is the guard alone on a machine where the poll was already
short; this is the guard *and* the poll on one where neither was, against a
baseline loop that is faster to begin with, so the same absolute saving is a
smaller fraction of it.

**The one-site companion is NOT resolved here**, and that matters because
site-scaling is how the Windows result was validated. `FieldLoop` was attempted
four times:

| attempt | floor | effect |
|---|---:|---:|
| 12 rounds, reps 8000 | 18.2% | −10.7% |
| 12 rounds, reps 25000 | 5.0% | −4.3% |
| 14 rounds, reps 25000 | 3.8% | −3.1% |
| 14 rounds, reps 25000 | 2.1% | **+2.9%** |

Three negative, one positive, and the only one whose floor clears the bar is
the one that disagrees with the other three. The host sat at a load average
above 30 on 8 cores for most of them. **Open, and honestly open** — not "flat,
as predicted".

It is also not the same experiment as the Windows one, which is why a flat
result would not have meant the same thing: this lever shortens the **back-edge
poll** as well as the guards, and every loop has a poll whether or not it has
four guarded field reads. A one-site loop here is not predicted to be flat the
way it was there.

**It ships default OFF anyway**, and that is a deliberately conservative call
rather than a doubt about the number. This moves where every JIT code buffer in
the process lives, which is a bigger surface than an encoding switch: it wants
a differential run against the Spring, H2 and WildFly suites before it becomes
what every Linux user gets. The fast regression suite is green with it on
(`CV=… CRATONVM_JIT_CODE_NEAR_GLOBALS=1 bash regression-suite/run.sh`), and
`cargo test -p cratonvm-jit` is 2347/2347, which is the floor for turning it on
deliberately — not the ceiling for turning it on by default.

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

On Linux, add `CRATONVM_JIT_CODE_NEAR_GLOBALS=1` to both arms — without it the
flag under test changes nothing there, because neither encoding is reachable:

```bash
# does the short form engage at all on this host?
for f in 0 1; do
  CRATONVM_JIT_CODE_NEAR_GLOBALS=$f CRATONVM_JIT_FORCE_C2=1 \
  CRATONVM_DBG=jit-disasm CRATONVM_DBG_JIT_DISASM=MultiFieldLoop.sumGuarded \
    ./target/release/cratonvm -cp /tmp/pc -Dprobe.reps=3000 MultiFieldLoop 2>&1 \
    | awk '/full\/ir MultiFieldLoop/{n++} n==1' | grep -c 813d
done
```
