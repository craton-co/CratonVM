# The optimizing tier's GP register file is not the binding constraint

**2026-09-10.** The tiering inversion on the field-read loop reproduces at
**1.594x** on current `dev`. The obvious next lever — the optimizing tier's
general-purpose register file is five registers wide while the single-pass
backend's is **seven** on Win64 — was built, engaged, and **measured slower**.
It ships default-OFF behind `CRATONVM_JIT_IR_GP_WIDE`, and this file is why.

## 1. The inversion, re-measured rather than quoted

`docs/JIT_OPTIMIZATION.md` recorded the field-read loop at ~1.65x on
2026-09-03. Since then `ir-drop-home`, `ir-reg-authoritative`,
`ir-linear-scan`, `ir-deopt-regs`, `ir-phi-copy-regs`, `ir-carry-single-use`
and `ir-drop-phi-home` all went **default-ON**, and
`docs/jit/linear-scan-wiring.md` still says the linear-scan path is default
off. It is not. So the number was retaken rather than carried forward.

`probes/FieldLoop.java` `sum`, `probe.reps=25000`, one release binary, arms
interleaved BAAB/ABBA with a second copy of the baseline arm as the control,
`tools/tier-ab/tier-ab.sh`:

| arm | median |
|---|---:|
| A — baseline (`CRATONVM_C2_SUPERSEDE=0`) | 503 ms |
| C — control, identical to A | 516 ms |
| B — optimizing (`CRATONVM_JIT_FORCE_C2=1`) | **812 ms** |

Noise floor (A vs C) **2.6%**; effect **+59.4%**, i.e. **1.594x**. Checksums
identical (`acc=1500150000`) in all twenty runs. The inversion is intact, and
the register work that landed since did not close it.

## 2. What the census said, and why the file looked like the answer

`CRATONVM_DBG_IR_LINEAR_SCAN=1` on that method:

```text
nodes=23 positions=17 peak_live=11 scan_promoted=9
resident=5 (fp=0 gp=5) demoted=0 splits=5 scan_spills=1 scan_reloads=2
skipped: split_or_spilled=1 ... spilled=1 no_alloc=4 carried_reserved=3
home: droppable=0 blocked_deopt=3 blocked_phi=2 safepoints=16
```

`resident=5` against a file of five: **every register handed out**, with
`peak_live=11`. That is a file exhausted, not a policy declining — which is a
different diagnosis from the four levers tried on 2026-09-03 (split residency,
implicit null checks, loop-weighted use counts, parameter prologue copies),
every one of which measured zero because the allocator had already declined
the values they argued about.

## 3. Two of seven registers were being left on the table, and the reason was a wrong comment

`regalloc::xmm_roles::IR_GP_LINEAR_SCAN` was `[3, 12, 13, 14, 15]` — RBX and
R12–R15 — on **both** platforms, and its doc gave the reason:

> **Callee-saved on both ABIs.** […] That rules out every caller-saved
> register, including the otherwise obvious System V candidates RSI/RDI.

That is a System V fact stated as an ABI-independent one. **Win64 makes RSI and
RDI callee-saved.** The single-pass backend has known this all along —
`x64::LOCAL_REGS` is `[u8; 7]` on Windows and `[u8; 5]` elsewhere — and
`RegFile::x86_64()` already builds them with `caller_saved: false`. Only the IR
tier's own constant was flat.

Nor were they otherwise spoken for on Windows: `CALL_ARG_REGS`,
`ENTRY_ABI_REGS` and `DEOPT_ARG0`/`1`/`2` are all `#[cfg]`-selected, and the
Windows arms are RCX/RDX/R8/R9. RSI/RDI are argument registers **only** on
System V — exactly the platform where they are also caller-saved. So the file
widens on Windows and does not move on System V, the same split
`IR_PROLOGUE_SAVED` already makes for XMM6/XMM7.

That disjointness is now a test rather than the paragraph above:
`the_gp_file_names_no_register_this_emitter_reserves` fails on either platform
if the file ever names a reserved register, and it was mutation-tested (putting
RCX in the file fails it) rather than assumed to be non-vacuous.

## 4. It engages

| | `GP_WIDE=0` | `GP_WIDE=1` |
|---|---:|---:|
| `sum` — resident | 5 (gp=5) | **6** |
| `sum` — splits | 5 | **2** |
| `sum` — scan_spills / reloads | 1 / 2 | **0 / 0** |
| `sum` — homes dropped | 3 | **4** |
| `sumWide` — resident | 5 | **7** |
| `sumWide` — splits | 27 | **14** |
| `sumWide` — spills / reloads | 10 / 14 | **7 / 9** |

More values resident, half the live-range splits, spills and reloads gone on
the narrow loop. On every census axis this is the change working.

## 5. And it is slower

`flag-ab.sh`, one binary, `CRATONVM_JIT_FORCE_C2=1` in both arms,
`probe.reps=25000`:

| arm | median |
|---|---:|
| A — `GP_WIDE=0` | 909 ms |
| C — control, identical to A | 903 ms |
| B — `GP_WIDE=1` | **948 ms** |

Noise floor **0.7%**, effect **+4.6%** — above the floor, checksums identical.
**Better residency, worse code.**

The frame is not the explanation, and that was designed in: `ir_gp_file()`
narrows the *handout*, and until the default flipped it did not narrow the
*reservation*, so the arms above were measured over identical frame layouts.
(The reservation now follows the handout as well, so that the OFF arm is
byte-identical to the pre-change tree — but the measurement that produced this
number was taken with both arms reserving seven.)

A second shape, `sumWide` (four independent accumulators — the shape a wider
file is *for*, and where residency went 5 → 7), read **+3.1% against an 8.8%
floor: UNMEASURABLE.** The host had gone busy; that run says nothing, and is
recorded so nobody reads its silence as agreement.

So the honest summary is: **one clean measurement saying slower, one noisy
measurement saying nothing.** Not "slower" as a settled fact — but nowhere is
there a measurement saying this pays.

## 6. Why more registers make it slower, which is the part worth keeping

`lower_data_node` gives every value a frame slot and **writes it**, and the
residency file is a read cache layered on top. `linear-scan-wiring.md` states
the consequence from the allocator's side — *"It buys loads, not stores. The
store side is still one per definition. That is the honest ceiling of this
increment."* — and `home: droppable=0` on this method says the ceiling is
exactly where the loop is.

Promoting a value therefore **adds** its publish move and **removes** no store.
A wider file buys more publishes against the same store traffic, and when the
extra candidates are ones the allocator then splits (`splits=14` even in the
best arm), it buys reload and copy instructions too. That is a cost curve that
gets worse with file width, not better, and it is why the fifth attempt in
`JIT_OPTIMIZATION.md` — a register-to-register publish — produced
byte-identical code: *"no policy change on the cache can remove a store the
model emits unconditionally."*

This measurement is that thesis approached from the opposite direction. The
earlier work showed residency cannot be *increased* usefully one value at a
time. This shows it cannot be increased usefully *in bulk* either, while the
frame stays authoritative.

## 7. What this retires, and what it leaves

**Retired: "the optimizing tier is short of registers."** It is short of two
registers on Win64 and giving them to it is measurably not an improvement. Any
future argument of the form "widen the file" needs to answer this measurement
first.

**Left standing, and now with one more independent witness:** the home slot has
to become optional before register residency pays anything. That is the change
`JIT_OPTIMIZATION.md` calls "an architecture change, not another heuristic" —
`emit_safepoint_map`, `build_deopt_points` and `emit_phi_copies` all have to be
able to read a **register**, which needs a register bank in the oop map and in
the deopt frame reconstructor. `ir-drop-home` and `ir-deopt-regs` are the first
two pieces of that and they are already on; `droppable=0` on this loop says
they do not yet reach a loop-carried value whose home a deopt or a phi still
names (`blocked_deopt=3 blocked_phi=2`).

**The flag is the instrument for asking again.** When home elimination reaches
the point where `droppable` is non-zero on a counted loop,
`CRATONVM_JIT_IR_GP_WIDE=1` re-asks this question in one run, and the two
numbers above are what it has to beat.

## 8. A lead this turned up and did not chase: the layout epoch guard is loop-invariant

Counted off the `full/ir` disassembly of `FieldLoop.sum` taken for §2
(`CRATONVM_DBG=jit-disasm CRATONVM_DBG_JIT_DISASM=FieldLoop.sum`), the loop
body runs from the back-edge target `0x1d1` to the back edge at `0x386` and its
hot path is about **27 instructions**. The first four of them are
`ir_lower::emit_layout_epoch_guard`:

```text
1d1: mov  r11, 7FF75BB18AF4h   ; &layout_replace_epoch  -- 10 bytes of imm64
1db: mov  ecx, [r11]
1de: cmp  ecx, 0
1e4: jne  -> the jit_getfield helper
```

That is **~15% of the loop's instruction count and 25 of its bytes, per
iteration**, spent re-reading a **process-wide** counter that the guard's own
doc says "only a REPLACEMENT bumps — never a new class registration". It is
loop-invariant by construction, and the value it guards (a baked compact cell
offset) is a compile-time constant.

Two things make this more interesting than the register file:

* it is paid by **every inline field access in every method**, not only by
  loops, so it is not a loop-shape-specific lever;
* the single-pass tier's own body of this method (`osr/sp`, 4x unrolled) shows
  the same guard, so this is not a tier gap — it is a cost both tiers carry and
  neither hoists.

Three routes, cheapest first, none of them measured here:

1. **RIP-relative the compare.** `cmp dword [rip+disp32], imm32` is one
   instruction and ~10 bytes against four instructions and 25, and drops the
   R11 clobber. The back-edge poll already emits a RIP-relative `test byte
   [rel …]`, so the encoder can do it; the constraint is the ±2GB reach from
   the code buffer to the static, which needs a fallback to the imm64 form when
   it does not hold.
2. **Hoist it to the loop preheader.** Sound only if a layout replacement
   cannot be *applied* between the preheader and a given iteration — i.e. only
   with a re-check on the safepoint poll's slow path, which is where a
   replacement would land.
3. **Make it a dependency instead of a check.** Register the artifact against
   the layout epoch and let the existing `jit_cache.remove` /
   `invalidate_matching` path drop it on a bump, the way HotSpot handles an
   assumption. That deletes the guard from the body entirely and is the largest
   of the three.

**Chased the same day.** Route 1 landed and is worth **14.5%** on a four-site
loop against a 2.5% floor (and nothing on a one-site loop, which is how a
per-site cost is supposed to behave). Routes 2 and 3 are **refused**: both fail
on in-flight frames, because cache invalidation governs future entries to a
method and not a frame already executing one — the same fact that retired
artifact displacement. Full write-up, including the address measurement that
explains why the obvious encoding had never worked, in
[`c2-the-layout-epoch-guard-was-unreachable-by-rip-20260910.md`](../../internal/performance/c2-the-layout-epoch-guard-was-unreachable-by-rip-20260910.md).

## 9. Reproducing

```bash
cargo build --release -p cratonvm-cli
javac -d /tmp/pc probes/FieldLoop.java

# §1 — the inversion
bash tools/tier-ab/tier-ab.sh -Exe "$PWD/target/release/cratonvm" \
    -Cp /tmp/pc -Class FieldLoop -Rounds 5 -D probe.reps=25000

# §4 — engagement
CRATONVM_JIT_IR_GP_WIDE=1 CRATONVM_JIT_FORCE_C2=1 CRATONVM_DBG_IR_LINEAR_SCAN=1 \
  ./target/release/cratonvm -cp /tmp/pc -Dprobe.reps=200 FieldLoop 2>&1 | grep ir-ls

# §5 — the result
bash tools/tier-ab/flag-ab.sh -Exe "$PWD/target/release/cratonvm" \
    -Cp /tmp/pc -Class FieldLoop -Flag CRATONVM_JIT_IR_GP_WIDE \
    -Base "CRATONVM_JIT_FORCE_C2=1" -Rounds 6 -D probe.reps=25000
```

Both A/B numbers were taken on a contended 32-core Windows host; §1's floor was
2.6% and §5's 0.7%, and §5's `sumWide` companion run's was 8.8%, which is what
disqualified it. A floor above ~3% means the run is describing the machine.
