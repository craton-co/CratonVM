# SIGSEGV in compiled code during the Tomcat annotation scan — FIXED

| | |
|---|---|
| **Status** | ✅ **FIXED** — retired from `docs/known-issues/jit/` on 2026-08-05 |
| **Cause** | the same defect as the retired `arrays-sort-long-osr-miscompile` write-up: the OSR publication stripped the register home from every slot that is *ever* a cat-2 high half. Here the reused slot holds a **`byte[]` reference**, not an `int` |
| **Fixed by** | `14a274085` (2026-08-04) — `fix(jit): a reused cat-2 high-half slot must keep its OSR register home`. That commit was written for the `Arrays.sort(long[])` symptom and closed this one without either investigation knowing about the other |
| **Severity** | high — a hard crash on Windows, and **silent wrong data on Linux** |
| **HotSpot** | clean |
| **Original record** | filed 2026-08-04 as OPEN — "reproduced, narrowed, not root-caused" |

## What this record got wrong, and why it matters

The original write-up is accurate about everything it measured. Three of its
conclusions were still wrong, and each one is a trap worth naming:

1. **"It does not minimise."** It minimises to 130 lines with no Tomcat, no jar
   and no BCEL parser — `probes/OsrRefSlotReuseProbe.java`, added here.
2. **"No single method accounts for it ... an interaction between two or more
   compiled bodies."** There is no interaction. All four stage methods have the
   *same* hazardous shape, so denying any one of them just moves the crash to
   the next one; denying the class removes them all.
3. **"CratonVM: SIGSEGV, deterministic (5/5)" / "`--nojit` clean" ⇒ a
   miscompile.** Right conclusion, but the arm that would have localised it was
   never run: on Linux the *same* defect is not a crash at all. It silently sums
   the wrong bytes — and `AnnotationScanSplitProbe` throws `sink` away, so it
   cannot tell. A probe that does not check its own answer only detects the
   failures that happen to be fatal.

## Why the fix and this record missed each other by thirteen hours

The attribution is not an inference from the symptom — the dates settle it. This
record's last update is `d042b0ea2`, **2026-08-03 22:31 -0300** (its own
"Discovered 2026-08-04" is the UTC reading of that same evening). The fix,
`14a274085`, landed **2026-08-04 11:43 -0300**. Every binary this record measured
predates it, including the "with the gate below" row: that gate is the
bisect-lever work, which became `ef8c62e3a` at 15:31 the next day and does carry
`14a274085` as an ancestor — but the *build* that was run did not.

Two investigations, thirteen hours apart, on one defect: one reached it through
`Arrays.sort(long[])` and an `int` loop counter, the other through Tomcat's
annotation scan and a `byte[]`. Neither could see the other, because the symptoms
share nothing — a garbage array index versus a hard SIGSEGV — and the shared
cause is four levels down, in which slots a whole-method scan is allowed to name.

## Root cause

`x64::osr::publish_entry_metadata` strips the OSR register home from cat-2
high-half slots so the trampoline cannot seed a dead half over a live local that
shares its register. It took that slot set from `wide_local_high_halves`, a
**whole-method scan**: every `lstore N` / `dstore N` anywhere in the method marks
`N+1`.

Every stage of `probes/AnnotationScanSplitProbe` ends with the timing line, and
javac allocates its locals like this:

```
slot 0/1 : long t0
slot 2/3 : long sink
slot 4   : Iterator          <- the for-each temporary …
slot 5   : byte[] b          <- … and the live reference the loop reads
slot 6   : int i
...
72: lstore 4                 <- long ns = System.nanoTime() - t0
```

`lstore 4` makes slots 4 **and 5** a cat-2 pair in a range disjoint from the
loop, so the whole-method scan reports slot 5 — the `byte[]` the loop is reading
right now. Stripping its register home leaves the trampoline seeding only its
**frame** slot while the compiled body keeps reading its **register**
(`reg_for_local` is deliberately unaffected by the strip), so the loop runs on
whatever the caller left there.

The faulting instruction is the `arraylength` at the loop head. This listing is
from a pre-fix Windows build under `CRATONVM_DBG_JIT_DISASM` +
`CRATONVM_DBG_JIT_NAMES` — the strip itself dates to `9f8e04281` (2026-05-22),
so any binary in that window shows it:


```
8e6: mov  rax,r14          ; aload 5   — slot 5's register home
8e9: test rax,rax          ; the null check passes: the garbage is not zero
8ec: je   <npe>
8f2: mov  eax,[rax+0Ch]    ; arraylength — ARRAY_LENGTH_OFFSET = 12
```

with `r14 = 8`, i.e. `read at address 0x0000000000000014`, exactly the address
the original record printed. Every other local was seeded correctly (`r15` and
`r13` hold live object pointers, `r12` the loop counter, `rbx` the accumulator);
slot 5 alone was skipped.

### Why it crashed on Windows and merely lied on Linux

`x64::LOCAL_REGS` is `[R12,R13,R14,R15,RBX,RSI,RDI]` on Windows and
`[R12,R13,R14,R15,RBX]` on SysV, because RSI/RDI are callee-saved only in the
Win64 ABI. Two more register homes change *which* register slot 5 lands in and
therefore what junk it inherits from the caller: on Windows a small integer that
faults on the first dereference, on Linux a value that happened to survive the
null check and the bounds check and produce a wrong sum.

Measured, on the same commit with only the fix reverted:

| host | slot 5's home | `AnnotationScanSplitProbe` (discards its answer) | `OsrRefSlotReuseProbe` (checks it) |
|---|---|---|---|
| Windows | R14 | **1/5 SIGSEGV**, 4/5 exit 0 and unverified | **3/3 fail**: wrong checksum, then SIGSEGV in `arrayRead()J [osr]` |
| Linux | R13 | 3/3 *appear clean* — a plausible ms/ns-per-byte row | **3/3 fail**: `expected=-163840 got=-163427` |

Note the top-left cell. The original record recorded this crash as
"deterministic (5/5)"; on the current tree with only the fix reverted it is
**1 in 5**, because whether the loop faults depends on what the caller happened
to leave in slot 5's home register — a detail that moves with every unrelated
change to the VM. A probe that only fails when the corruption is fatal is a
coin-flip regression guard. That is the argument for the checksum, not a
stylistic preference.

The Linux row is the reason this is filed as high severity rather than as a
crash bug. `arrayRead` also went from 1 ms to 13 ms there — the only visible
signal, in a column nobody was reading as a correctness check.

## The minimised reproducer

`probes/OsrRefSlotReuseProbe.java`. Two ingredients the original minimisation
attempts left out, and neither is the workload:

* **the trailing `long ns = System.nanoTime() - t0;`** — that one statement is
  the `lstore 4` that makes slot 5 a high half. Drop it and the shape is safe;
* **`main` must not go hot first.** The OSR trigger fires on a back-edge taken
  *by the interpreter*. If `main` builds its fixture in a hot loop, `main` OSRs,
  and every stage it then calls is entered from compiled code and compiled at
  its **entry** instead — no OSR entry, no bug. That is the whole of `parseMem`'s
  apparent load-bearing role: not the BCEL parser, just a fixture that arrives
  without `main` going hot. The probe keeps its fill loops in `setup()` and
  `main` has no loop of its own.

```bash
javac -d /tmp/out probes/OsrRefSlotReuseProbe.java
<cratonvm> --java-home <real JDK 25> -Xmx2g -cp /tmp/out OsrRefSlotReuseProbe
```

It exits 0 with `failures=0` on HotSpot and on CratonVM at this commit, and
fails (or dies) with the fix reverted. It verifies a checksum over a fixed byte
pattern, so a wrong read is as loud as a crash — which is what the original
probe was missing.

## What landed with this record

* **`probes/OsrRefSlotReuseProbe.java`** — the minimisation above.
* **`x64::bce::pure_high_halves`** — the strip filter now has one name, called by
  the publication and asserted directly by the tests. Both tests previously
  re-implemented the predicate, which passes while the call site drifts.
* **`a_high_half_reused_as_a_reference_keeps_its_osr_register_home`** — the
  sibling of the existing `int`-reuse test. The two arrive through different
  opcode families (`aload`/`astore` vs `iload`/`istore`) and different arms of
  `local_access_at`, so one passing is not evidence for the other. Verified by
  injection: replacing the filter with `|_| true` fails both.

No behavioural change: the fix itself is `14a274085`, already on `dev`.

## Verification

All on `dev` at `4192dec7c` / `da8e2cf06`, real JDK 25, `-Xmx2g`.

| arm | Windows | Linux |
|---|---|---|
| `AnnotationScanSplitProbe`, 156 classes, unmodified | 3/3 clean | 3/3 clean |
| `AnnotationScanSplitProbe`, fix reverted | 1/5 SIGSEGV | 3/3 clean (and silently wrong) |
| `OsrRefSlotReuseProbe`, unmodified | 3/3 pass | 3/3 pass |
| `OsrRefSlotReuseProbe`, fix reverted | 3/3 fail (checksum, then SIGSEGV) | 3/3 fail (3 wrong checksums) |
| HotSpot 25 control | pass | pass |
| `cargo test -p cratonvm-jit` | — | 2201 pass, 0 fail |

**The clean arms are not inert.** `CRATONVM_DBG=osr,osr-meta` on the passing
runs shows all four stage methods still OSR-entering at the same PCs the crash
report named — `readBytes()J entry_pc=59`, `readRaw()J entry_pc=52`,
`arrayRead()J entry_pc=41`, `allocOnly()J entry_pc=41` — and the published
`gpr_resident` masks confirm slot 5 keeps its register home (`arrayRead`:
`0x75` fixed vs `0x55` reverted, the missing bit being slot 5). That check is
this record's own advice, taken: *verify each lever changed the compiled census
before reading anything into a "no effect" row.*

## The bisect levers

The original record's other finding stands and is unaffected by this one:
`compile_osr_artifact` reached `x64::compile_with_param_slots` directly, so
`CRATONVM_JIT_DENY` / `CRATONVM_JIT_BISECT_ONLY` could not force an OSR body to
interpret and a bisect against it read as a clean exoneration. That is fixed —
the levers are one shared predicate, `cratonvm_jit::jit_force_interpret`, applied
at `try_compile`, at `compile_osr_artifact` and at `execute`'s eager first-call
compile, and `compile_gate::admit` is now the single admission door for all
three.

Worth keeping from that half of the investigation: the flag was first spelled
`CRATONVM_JIT=bisect-only=…` when `bisect-only` is a `CRATONVM_DBG` token, the
VM said `unknown configuration token` on stderr, and an output filter ate it —
so every "no effect" row was an inert lever. Validating *one* token and assuming
the rest parsed is not validation.

## Relationship to the throughput work

Found while decomposing the deploy wall, and unrelated to it. The same probe's
timing output is what establishes that the annotation scan's cost is the
per-byte I/O call chain rather than object construction — see
`../../../known-issues/tomcat/!webapp-deploy-annotation-scan-interpreted-226x.md`,
which is unaffected and stays open.
