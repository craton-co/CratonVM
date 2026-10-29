# rx27 wave 4: per-row instruction breakdown of the regex/split rows still over 3x

Status: OPEN (measurement page; it names levers, it fixes nothing)
Area: JIT code quality of the big spliced IR bodies (`Pattern$Start.match`, `Pattern$BmpCharPropertyGreedy.match`,
`Pattern.split`, `Pattern.compile`, the driver)
Found: rx27 wave 4, lane prof (host), 2026-10-28. Binary `cvm-rx27b5` (`jit-regex-r27` 582534e766); HotSpot 25.0.4.
Parents: `r23-w2-regex-rows-gap-is-per-call-cost-20261021.md`, `r23-w5-compiled-jdk-string-split-and-regex-bodies-7-13x-20261021.md`

## Method

Tools are in `tools/probes/rx27-prof/`. The host copies are `/root/rx27/prof.sh`, `/root/rx27/ana2.py` and `/root/rx27/res.py`; the raw data is in `/root/rx27/prof/`.

- **Warmup and window.** `prof.sh <tag|hs> <row>` drives `R24RegexSplitRowsQ countq` exactly as `icount2.sh` does: warm-up, compile-clock quiesce, the pre window, then windows 0 and 1. Only window 2 (3N ops, every window clean) is recorded, with `perf record -e instructions:u -c 1000000 -t <Java main tid>`. This is the main thread only.
- **CratonVM** runs with `CRATONVM_JIT_PERF_MAP=1 CRATONVM_DBG_JIT_DISASM=1`. The disassembly carries real addresses, so every sample IP is joined to the instruction it hit in the last body published there.
- **HotSpot** runs with `-XX:+DumpPerfMapAtExit -XX:+PreserveFramePointer`.
- **Scaling.** Per-symbol instr/op is the sample share times the row's main/op from the orchestrator's latest `icount2`:
  - CratonVM: `def` arm of `ic2-t7`/`t8`, `b3` for `compile`.
  - HotSpot: `ic2-t5` and the wave-3 table.

  The first `-c 20000` runs were throttled by `perf_event_max_sample_rate=5000`, which is why the page uses shares rather than absolute sample counts.
- **Cause classes.** Each CratonVM instruction is classified by its text. A conditional jump takes the class of the compare in front of it.

| class | instruction shape | meaning |
|---|---|---|
| spill/frame-slot, compare on frame slot | `mov reg,[rbp-x]` / `mov [rbp-x],reg` / `cmp [rbp-x],..` | a value homed in the frame instead of a register, mostly store-then-reload chains inside loops (see the `Start.match` excerpt below) |
| frame-clear | `mov qword [rbp-x],0`, `vmovdqu [rbp-x],ymm` | reference-home clears: in prologues, and in `Pattern.split` once per loop iteration |
| reg move | `mov reg,reg` | argument shuffles around Java calls, copies |
| Java call via IC | `cmp eax,[r10]` / `mov r11,[r10+8]` / `btr r11,0` / `call r11` | inline-cache call to another compiled body |
| stack guard/thread state | `mov rax,[fs:..]`, `cmp rsp,[rax+430h]`, `movzx eax,sp` | per-call stack-overflow check, and each poll's first instruction |
| frame-addr (helper window) | `lea rcx,[rbp-x]` + `mov [r11+..],rcx` | frame addresses published before a call |
| safepoint poll | `test byte [r11],0FFh` + `jne` | back-edge poll |
| layout guard (hdr bit) | `test byte [obj+7],4` + `je` | the per-object `GC_FLAG_COMPACT` test before every inline field access (`x64/op_field.rs`) |
| global flag check | `cmp dword [rel abs],0` / `mov ecx,[r11]; cmp ecx,0` | a process word tested before field and array accesses |
| bounds check | `cmp reg,[arr+8]` + `jae` | array bounds |
| class test (header word) | `mov eax,[obj]`, compare with the class word | type tests / IC guards |
| allocation | `[r10+2A0h]` TLAB top, `mov dword [obj+4],imm` header | inline allocation |
| compare/branch (control) | any other `cmp`/`test`/`jcc` | loop control, null tests, data tests (not split out) |

**Caveats.**
- Instruction samples skid by one or two instructions, so the class shares are reliable at the 5 % level, not per instruction.
- HotSpot inlines each row into one C2 body (the lambda's `run`, or one `Pattern` method). Its symbol column is coarse; only its total compares.
- The collectors differ. CratonVM ran its default GC (ZGC: `cratonvm_gc::zgc` frames appear in the profile); HotSpot ran G1 (its `G1BarrierSet` entries show on the split rows). Only the "global flag check" class (3-5 % of a row) could be collector-related.

## Round rx27 wave 4 (lane guards, 2026-10-28; pending build): the per-iteration guards

Code only, measured by the prof lane. Each lever has its own opt-in switch; the rules live in `jit/src/rx27_guards.rs`.

**(a) The JDK table re-read.** `ASCII.ctype` and `CharacterDataLatin1.A` are `static final int[]` of boot classes.
Every static-final fold trusted application and user-loader classes only, so each character re-read the static,
null-checked it, loaded its length and bounds-checked it.
- `CRATONVM_JIT_TRUST_BOOT_PRIM_ARRAY_FINALS` trusts a boot or platform class's `static final` PRIMITIVE array
  (class file 53+, not `java/lang/System`).
  - Its length is filed at the end of `<clinit>` (vm `note_class_initialized_final_array_lengths`).
  - The compile's trusted list answers it (vm `static_field_is_trusted_final`).
  - Its reads leave loops, spliced sites included (`loop_static_hoist::trusted_final_static_reads`).
  - Sound for these reasons: from class file 53 a `putstatic` of a final outside `<clinit>` is an
    `IllegalAccessError`; JLS 17.5.3 lets a compiler ignore reflective and `Unsafe` writes (C2 folds every
    static final); the boot rewrites the old exclusion names are reference fields (`System`'s streams, still
    excluded); and redefinition never re-runs `<clinit>`.
- `CRATONVM_JIT_IR_BCE_CONST_LENGTH_RANGE` drops a bounds check when two things hold: the array's length is
  known at compile time (a fresh `new T[c]`, or a filed trusted static length; `ir_check_elim::const_array_length`),
  and the index's path-sensitive range at the access lies in `[0, L-1]`. Target shapes: `ASCII.getType`'s
  `ctype[ch]` under `ch < 128`, and `A[ch & 0xFF]`.

**(b) Two polls per trip.** A poll goes on every edge whose target is laid out at or before its source. After
frequency layout that includes forward CFG edges laid out backwards.
- `CRATONVM_JIT_IR_POLL_TRUE_BACK_EDGES` drops such an edge's poll (`ir_lower::poll_on_layout_back_edge`).
- It does so only when the CFG is reducible and every dominator back edge is itself laid out backwards. Every
  cycle then keeps one poll.
- Counted inner loops still poll; dropping those as C2 does needs strip-mining and a TTSP bound, and is not done.

**(c) The compact-layout test and the global word.** Not changed this wave; see
`rx27-guards-compact-field-test-residual-20261028.md`. The proof machinery
(`CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS*`, all default ON) already hoists the test per receiver. Where it is
still per character, it is a site the proof refused, and the census below names it. The global word is the
layout-epoch guard and the ZGC read-barrier table check, both GC-owned.

Engagement (`cd /root/rx27/p`, `B=/root/r25-bin/cvm-<tag>`, all three switches `=1`):
```
G="CRATONVM_JIT_TRUST_BOOT_PRIM_ARRAY_FINALS=1 CRATONVM_JIT_IR_BCE_CONST_LENGTH_RANGE=1 CRATONVM_JIT_IR_POLL_TRUE_BACK_EDGES=1"
env $G CRATONVM_DBG_JITC=1 CRATONVM_DBG_LICM=1 $B --jdk-only -cp . R24RegexSplitRows matchonly 1 3 2> g.txt
grep 'static final array length filed' g.txt | sort -u | head   # ASCII / CharacterDataLatin1 rows (class ids)
grep -c 'HOIST to preheader' g.txt                               # the table read leaves the scan loop
grep -c 'bce const-length range PROVED' g.txt                    # bounds checks removed by (a)
grep -c 'poll-true-back-edges: forward edge' g.txt               # polls removed by (b)
grep 'ir proven-receiver-fields' g.txt | grep -E 'Start.match|Matcher' # (c): full= / establish= per body
```
Arms (`icount2.sh rx27g matchonly,regex-matches,regex-find-email,regex-find-ci,regex-groups,split-ws,cached,compile`):
`hs:hs:` `def:<tag>:` `tbl:<tag>:CRATONVM_JIT_TRUST_BOOT_PRIM_ARRAY_FINALS=1`
`bce:<tag>:CRATONVM_JIT_TRUST_BOOT_PRIM_ARRAY_FINALS=1,CRATONVM_JIT_IR_BCE_CONST_LENGTH_RANGE=1`
`poll:<tag>:CRATONVM_JIT_IR_POLL_TRUE_BACK_EDGES=1`
`all:<tag>:CRATONVM_JIT_TRUST_BOOT_PRIM_ARRAY_FINALS=1,CRATONVM_JIT_IR_BCE_CONST_LENGTH_RANGE=1,CRATONVM_JIT_IR_POLL_TRUE_BACK_EDGES=1`.

Expected:
- `tbl` and `bce` remove the per-character static re-read, null check, length load and bounds check of the
  table: about 0.5k plus the bounds share (0.8-1.2k) on the scan rows.
- `poll` removes the second poll where layout made it: up to the poll class (0.6-0.8k).
- No fact changes are expected. Batteries: `R24RegexSplitRows facts`, the regex facts probes, and a
  `Character` / `ASCII` table probe in both modes. Also the GC battery under `all`: hoisted references now live
  across the loop's polls; they are roots of static finals, which never move.

### Guards lane, host results (prof agent, 2026-10-28/29)

**b8** (int8, before the mask-guard fix `adcf08c8e0`). icount2 main/op, median of 5. No arm moves a row beyond
noise. `gall` is best on `matchonly` (25 457 against def 27 298, -6.7 %) and `regex-find-email` (-1.7 %); neither
range is disjoint from def. `compile` 4 348 / 4 393. Facts are equal to HotSpot.

**Why b8's `bce` did nothing.** The never-taken pruning turns `ASCII.getType`'s `(ch & 0xFFFFFF80) == 0 ?
ctype[ch] : 0` into `Guard((ch & -128) == 0)` and a straight-line read (one-block `isSpace` / `isType` graphs). The
first mask rule (`fc4947031a`) read If edges only. `adcf08c8e0` adds the Guard form
(`ir_check_elim::mask_zero_guard_facts`).

**Polls.** `Pattern$Start.match` has two real per-character loops: the position loop at bci 31 and the spliced
greedy scan at bci 99. `CRATONVM_JIT_IR_POLL_TRUE_BACK_EDGES` therefore has nothing to drop on these rows. Dropping
a real loop's poll needs counted-loop strip-mining and a time-to-safepoint bound, which is not done.

**b9** (with `adcf08c8e0`), `gall` on `matchonly`:
- `bce const-length range PROVED` 7 times, all `length=128 mask_max=Some(127)`: `ASCII.isSpace`, `ASCII.isType`,
  `CharPredicates.lambda$ASCII_SPACE$0`, `Matcher.search` x2, `Pattern$Start.match` (lattice range [0, 255]).
- `HOIST to preheader` 5.
- Facts 34/34 equal HotSpot in both modes, GC battery included.
- **b9 icount2** (main/op, median of 5): no arm moves a row beyond noise. Medians against def:
  - `cached` under bce: 36 982 against 39 941 (-7.4 %), but the ranges overlap (34 867-39 292 against
    37 493-40 903).
  - `regex-find-email` under tbl: -1.6 %, overlapping.
  - `matchonly`: def is bimodal (25 178-31 874), and bce / gall read higher by median.
  - `compile`: 4 349-4 436 on every arm.

**Verdict (guards lane, 2026-10-29): nothing to flip; all three switches were later REMOVED (`5ea831d133`; close-out on `rx27-guards-compact-field-test-residual-20261028.md`).** Each proved `ctype` check
is 2-3 instructions per character inside the spliced predicate, and the hoisted static read is 2 loads. Both are
below this host's noise on these rows. The guard classes the profile counted, 2.0-6.7k per row, are mostly the
compact-layout test and the global word (`rx27-guards-compact-field-test-residual-20261028.md`, GC-owned) and
two real loops' polls. Those are the remaining levers, not this lane's switches.

## Findings across rows

1. **Values live in the frame, not in registers.** Frame-slot traffic plus compares on frame slots is the largest single class on every row: 4.5k-9.9k instr/op, 20-33 % of each row. The `Start.match` scan loop of `matchonly` runs once per character and shows the shape:
   - `mov [rbp-98h],rax` / `cmp dword [rbp-98h],80h` / `mov rax,[rbp-98h]`
   - `and eax,0FFFFFF80h` / `mov [rbp-0A8h],rax` / `cmp dword [rbp-0A8h],0`

   Every intermediate value of the spliced `BmpCharPropertyGreedy` / `CharPredicates` predicate gets a frame home and is stored, then reloaded. HotSpot keeps them all in registers.
2. **Per-iteration work that HotSpot hoists or never emits.**
   - `CharacterDataLatin1.A` is reread from its static on every character (`mov rax,imm` / `mov rax,[rax]` / `mov rax,[rax+0F8h]`), then null-checked and bounds-checked.
   - Each iteration has two safepoint polls (TLS load, `test`, `lea`, `test byte`).
   - Every field access tests the `GC_FLAG_COMPACT` header bit and the global word.

   Together the guard classes cost 2.0k-6.7k instr/op per row.
3. **Frame clears.** The bodies' frames are 600-1300 bytes, and their prologues clear dozens of slots. `Pattern.split` also runs a block of 13 `mov qword [rbp-x],0` (offsets `+9dd..+a61`) on every `find` iteration of its spliced find loop. That block is most of `split`'s 4.5k-5.1k frame-clear instr/op on `split-ws` and `cached`.

   For the bimodal lane: its prologue / frame-clear work reaches the prologue clears. The per-iteration block is a different site: clears at the entry of a spliced region inside a loop.
4. **Calls HotSpot inlines.** `regex-matches` calls `Pattern.matcher` and then `BmpCharPropertyGreedy.match` once per subject, through the IC. Each call adds a stack guard, the frame-address publish and argument shuffles (`reg move` 1.2k-4.3k instr/op).
5. **No runtime helper is hot.**
   - `jit_getfield` / `jit_putfield_int` appear in `Start.match` only on the never-taken slow arm of the compact-layout guard.
   - `jit_checkcast` costs 99 instr/op on `compile` (2.3 %).
   - GC runtime is under 1.5 % of every row.
   - No interpreter or C1 time: every sampled body is `full/ir`.

## Per-row tables

The CratonVM columns hold instr/op of that symbol on the main thread. The cause-class column lists the symbol's four largest classes.

### regex-matches: CratonVM 16474 vs HotSpot 3798 main/op (4.3x)

| CratonVM symbol | instr/op | share | frame | cause classes (instr/op) |
|---|---:|---:|---:|---|
| `Pattern$BmpCharPropertyGreedy.match` | 7221 | 43.8% | 928 B | spill/frame-slot 1950; frame-clear 1155; compare/branch (control) 867; heap load/store 505 |
| `Pattern.matcher` | 3996 | 24.3% | 672 B | compare/branch (control) 1119; spill/frame-slot 759; allocation (TLAB bump/header) 400; heap load/store 320 |
| `spin (driver + inlined row lambda)` | 3769 | 22.9% | 800 B | compare/branch (control) 1055; spill/frame-slot 791; layout guard (hdr bit) 415; reg move (arg shuffle/copy) 377 |
| `Pattern$LastNode.match` | 865 | 5.2% | 208 B | spill/frame-slot 355; reg move (arg shuffle/copy) 156; stack guard/thread state 130; frame-clear 61 |
| `CharPredicates.lambda$ASCII_DIGIT$0` | 361 | 2.2% | 488 B | reg move (arg shuffle/copy) 119; alu/other 79; spill/frame-slot 65; frame-clear 43 |
| `[unknown]` | 95 | 0.6% | - | runtime (Rust) |

Row totals by class (top-10 JIT symbols): spill/frame-slot 3920, compare/branch (control) 3079, frame-clear 1647, reg move (arg shuffle/copy) 1213, heap load/store 977, layout guard (hdr bit) 791, frame-addr (helper window) 625, safepoint poll 589, jmp (block layout) 573, compare on frame slot 566.

HotSpot 25 (G1, C2; everything inlined, so symbols are coarse): `long R24RegexSplitRows$$Lambda/0x000000007d041000.run` 3533, `StubRoutines` 226, `[unknown]` 20, `long spin (driver + inlined row lambda)` 16.

### compile: CratonVM 4381 vs HotSpot 800 main/op (5.5x)

| CratonVM symbol | instr/op | share | frame | cause classes (instr/op) |
|---|---:|---:|---:|---|
| `Pattern.compile` | 1748 | 39.9% | 1248 B | compare/branch (control) 420; frame-clear 297; spill/frame-slot 262; heap load/store 210 |
| `Pattern.sequence` | 700 | 16.0% | 1240 B | spill/frame-slot 168; frame-clear 126; frame-addr (helper window) 98; heap load/store 91 |
| `Pattern.escape` | 360 | 8.2% | - | spill/frame-slot 108; heap load/store 76; compare/branch (control) 65; frame-clear 61 |
| `Pattern.RemoveQEQuoting` | 261 | 6.0% | 1080 B | spill/frame-slot 50; alu/other 42; compare/branch (control) 39; reg move (arg shuffle/copy) 31 |
| `Pattern.atom` | 259 | 5.9% | 1296 B | frame-clear 98; heap load/store 39; spill/frame-slot 36; reg move (arg shuffle/copy) 23 |
| `Pattern.expr` | 219 | 5.0% | 832 B | spill/frame-slot 53; frame-clear 48; reg move (arg shuffle/copy) 20; heap load/store 18 |
| `Pattern.curly` | 166 | 3.8% | 1032 B | compare/branch (control) 33; heap load/store 32; spill/frame-slot 28; frame-addr (helper window) 25 |
| `Pattern.closure` | 141 | 3.2% | 1120 B | heap load/store 42; frame-addr (helper window) 38; spill/frame-slot 28; frame-clear 16 |
| `cratonvm_vm::jit::helpers::jit_checkcast` | 99 | 2.3% | - | runtime (Rust) |
| `Pattern$BmpCharPropertyGreedy.study` | 82 | 1.9% | - | compare/branch (control) 20; spill/frame-slot 19; heap load/store 10; alu/other 10 |

Row totals by class (top-10 JIT symbols): spill/frame-slot 752, frame-clear 670, compare/branch (control) 649, heap load/store 540, reg move (arg shuffle/copy) 308, frame-addr (helper window) 210, alu/other 156, compare on frame slot 128, layout guard (hdr bit) 116, jmp (block layout) 93.

HotSpot 25 (G1, C2; everything inlined, so symbols are coarse): `void java.util.regex.Pattern.compile` 783, `[unknown]` 5, `java.util.regex.Pattern$Node java.util.regex.Pattern.sequenc` 5, `void java.util.regex.Pattern.RemoveQEQuoting` 4.

### matchonly: CratonVM 27075 vs HotSpot 7020 main/op (3.9x)

| CratonVM symbol | instr/op | share | frame | cause classes (instr/op) |
|---|---:|---:|---:|---|
| `Pattern$Start.match` | 19535 | 72.2% | 720 B | compare/branch (control) 5274; spill/frame-slot 4493; compare on frame slot 2344; reg move (arg shuffle/copy) 1367 |
| `spin (driver + inlined row lambda)` | 6819 | 25.2% | 848 B | compare/branch (control) 2114; spill/frame-slot 1364; layout guard (hdr bit) 886; compare on frame slot 614 |
| `Pattern.matcher` | 596 | 2.2% | 672 B | compare/branch (control) 191; reg move (arg shuffle/copy) 66; spill/frame-slot 60; heap load/store 48 |
| `[unknown]` | 106 | 0.4% | - | runtime (Rust) |

Row totals by class (top-10 JIT symbols): compare/branch (control) 7579, spill/frame-slot 5916, compare on frame slot 2982, reg move (arg shuffle/copy) 1910, layout guard (hdr bit) 1899, global flag check (barrier) 1561, jmp (block layout) 1543, bounds check 781, safepoint poll 728, alu/other 471.

HotSpot 25 (G1, C2; everything inlined, so symbols are coarse): `long R24RegexSplitRows$$Lambda/0x000000003f041000.run` 6002, `boolean java.util.regex.Pattern$LastNode.match` 630, `StubRoutines` 321, `long spin (driver + inlined row lambda)` 36.

### regex-groups: CratonVM 22728 vs HotSpot 5764 main/op (3.9x)

| CratonVM symbol | instr/op | share | frame | cause classes (instr/op) |
|---|---:|---:|---:|---|
| `Pattern$Start.match` | 5958 | 26.2% | 800 B | compare/branch (control) 1490; spill/frame-slot 1192; reg move (arg shuffle/copy) 536; compare on frame slot 536 |
| `spin (driver + inlined row lambda)` | 5182 | 22.8% | 928 B | compare/branch (control) 1762; spill/frame-slot 881; layout guard (hdr bit) 570; reg move (arg shuffle/copy) 415 |
| `Pattern$BmpCharPropertyGreedy.match` | 4638 | 20.4% | 880 B | spill/frame-slot 1206; frame-clear 696; compare/branch (control) 557; reg move (arg shuffle/copy) 464 |
| `Matcher.group` | 3590 | 15.8% | 720 B | heap load/store 1041; compare/branch (control) 969; reg move (arg shuffle/copy) 323; spill/frame-slot 251 |
| `Pattern$BmpCharProperty.match` | 1939 | 8.5% | 696 B | spill/frame-slot 368; compare/branch (control) 368; heap load/store 175; reg move (arg shuffle/copy) 175 |
| `Pattern.matcher` | 711 | 3.1% | 672 B | compare/branch (control) 256; reg move (arg shuffle/copy) 71; spill/frame-slot 71; jmp (block layout) 50 |
| `Pattern$LastNode.match` | 528 | 2.3% | 328 B | spill/frame-slot 380; frame-clear 42; alu/other 37; reg move (arg shuffle/copy) 26 |
| `[unknown]` | 164 | 0.7% | - | runtime (Rust) |

Row totals by class (top-10 JIT symbols): compare/branch (control) 5407, spill/frame-slot 4349, reg move (arg shuffle/copy) 2010, heap load/store 2000, layout guard (hdr bit) 1403, frame-clear 1216, compare on frame slot 1112, jmp (block layout) 1014, global flag check (barrier) 996, alu/other 673.

HotSpot 25 (G1, C2; everything inlined, so symbols are coarse): `boolean java.util.regex.Pattern$BmpCharPropertyGreedy.match` 2739, `long R24RegexSplitRows$$Lambda/0x0000000068041000.run` 2461, `StubRoutines` 493, `[unknown]` 68.

### regex-find-email: CratonVM 38598 vs HotSpot 9679 main/op (4.0x)

| CratonVM symbol | instr/op | share | frame | cause classes (instr/op) |
|---|---:|---:|---:|---|
| `Pattern$Start.match` | 26114 | 67.7% | 880 B | compare/branch (control) 9923; spill/frame-slot 2873; reg move (arg shuffle/copy) 2873; compare on frame slot 2350 |
| `Pattern$BmpCharPropertyGreedy.match` | 9091 | 23.6% | 944 B | compare/branch (control) 2455; spill/frame-slot 1818; reg move (arg shuffle/copy) 1182; frame-clear 818 |
| `spin (driver + inlined row lambda)` | 1575 | 4.1% | 864 B | compare/branch (control) 488; spill/frame-slot 220; layout guard (hdr bit) 158; compare on frame slot 158 |
| `Pattern.lambda$Range$0` | 733 | 1.9% | 544 B | alu/other 279; heap load/store 88; frame-clear 88; compare on frame slot 81 |
| `Pattern.matcher` | 598 | 1.5% | 672 B | compare/branch (control) 161; reg move (arg shuffle/copy) 84; spill/frame-slot 60; allocation (TLAB bump/header) 48 |
| `Pattern$LastNode.match` | 264 | 0.7% | 208 B | spill/frame-slot 69; alu/other 53; reg move (arg shuffle/copy) 29; heap load/store 26 |
| `[unknown]` | 149 | 0.4% | - | runtime (Rust) |
| `lambda-adapter->0x7bf900979880 [stub:lambda-adapter]` | 73 | 0.2% | - | runtime (Rust) |

Row totals by class (top-10 JIT symbols): compare/branch (control) 13068, spill/frame-slot 5113, reg move (arg shuffle/copy) 4300, compare on frame slot 3425, global flag check (barrier) 1897, jmp (block layout) 1785, layout guard (hdr bit) 1513, class test (header word) 1323, heap load/store 1266, bounds check 1232.

HotSpot 25 (G1, C2; everything inlined, so symbols are coarse): `boolean java.util.regex.Pattern$BmpCharPropertyGreedy.match` 7732, `long R24RegexSplitRows$$Lambda/0x000000001a041000.run` 1832, `StubRoutines` 83, `[unknown]` 17.

### regex-find-ci: CratonVM 26994 vs HotSpot 7330 main/op (3.7x)

| CratonVM symbol | instr/op | share | frame | cause classes (instr/op) |
|---|---:|---:|---:|---|
| `Pattern$Start.match` | 24090 | 89.2% | 800 B | compare/branch (control) 9636; spill/frame-slot 4818; compare on frame slot 2168; layout guard (hdr bit) 1445 |
| `spin (driver + inlined row lambda)` | 2163 | 8.0% | 848 B | compare/branch (control) 584; spill/frame-slot 454; layout guard (hdr bit) 260; reg move (arg shuffle/copy) 195 |
| `Pattern.matcher` | 639 | 2.4% | 672 B | compare/branch (control) 217; spill/frame-slot 77; heap load/store 51; reg move (arg shuffle/copy) 51 |
| `[unknown]` | 98 | 0.4% | - | runtime (Rust) |

Row totals by class (top-10 JIT symbols): compare/branch (control) 10437, spill/frame-slot 5349, compare on frame slot 2367, layout guard (hdr bit) 1731, reg move (arg shuffle/copy) 1209, jmp (block layout) 1104, heap load/store 1058, alu/other 998, global flag check (barrier) 884, safepoint poll 772.

HotSpot 25 (G1, C2; everything inlined, so symbols are coarse): `long R24RegexSplitRows.lambda$row$2` 7106, `StubRoutines` 161, `[unknown]` 36, `long spin (driver + inlined row lambda)` 25.

### split-ws: CratonVM 45438 vs HotSpot 12600 main/op (3.6x)

| CratonVM symbol | instr/op | share | frame | cause classes (instr/op) |
|---|---:|---:|---:|---|
| `Pattern$Start.match` | 19310 | 42.5% | 680 B | spill/frame-slot 4248; compare/branch (control) 4055; reg move (arg shuffle/copy) 1545; heap load/store 1352 |
| `Pattern.split` | 15845 | 34.9% | 1296 B | frame-clear 5070; compare/branch (control) 3169; heap load/store 2218; spill/frame-slot 1901 |
| `Pattern$LastNode.match` | 2563 | 5.6% | 208 B | spill/frame-slot 948; reg move (arg shuffle/copy) 487; compare/branch (control) 282; jmp (block layout) 179 |
| `Pattern.compile` | 1581 | 3.5% | 1232 B | frame-clear 364; spill/frame-slot 332; compare/branch (control) 221; reg move (arg shuffle/copy) 190 |
| `ArrayList.grow` | 757 | 1.7% | 656 B | frame-clear 159; spill/frame-slot 114; compare/branch (control) 98; heap load/store 76 |
| `Pattern.sequence` | 653 | 1.4% | 1288 B | spill/frame-slot 137; frame-clear 131; heap load/store 85; frame-addr (helper window) 85 |
| `Pattern.matcher` | 613 | 1.3% | 672 B | compare/branch (control) 141; reg move (arg shuffle/copy) 98; heap load/store 92; spill/frame-slot 61 |
| `Pattern.escape` | 467 | 1.0% | 1480 B | frame-clear 163; heap load/store 135; spill/frame-slot 98; compare/branch (control) 23 |
| `Arrays.copyOf` | 444 | 1.0% | 808 B | heap load/store 147; spill/frame-slot 142; compare/branch (control) 44; alu/other 27 |
| `spin (driver + inlined row lambda)` | 369 | 0.8% | 880 B | frame-clear 111; reg move (arg shuffle/copy) 70; compare/branch (control) 59; spill/frame-slot 52 |

Row totals by class (top-10 JIT symbols): compare/branch (control) 8139, spill/frame-slot 8034, frame-clear 6701, heap load/store 4313, reg move (arg shuffle/copy) 3646, compare on frame slot 1844, jmp (block layout) 1631, layout guard (hdr bit) 1236, bounds check 1215, stack guard/thread state 1162.

HotSpot 25 (G1, C2; everything inlined, so symbols are coarse): `long R24RegexSplitRows$$Lambda/0x000000002a041000.run` 9449, `StubRoutines` 1438, `void java.util.regex.Pattern.compile` 930, `boolean java.util.regex.Pattern$LastNode.match` 499.

### cached: CratonVM 38774 vs HotSpot 11800 main/op (3.3x)

| CratonVM symbol | instr/op | share | frame | cause classes (instr/op) |
|---|---:|---:|---:|---|
| `Pattern$Start.match` | 17298 | 44.6% | 680 B | spill/frame-slot 3979; compare/branch (control) 3806; compare on frame slot 1557; reg move (arg shuffle/copy) 1557 |
| `Pattern.split` | 16610 | 42.8% | 1312 B | frame-clear 4485; compare/branch (control) 3654; heap load/store 2492; spill/frame-slot 2159 |
| `Pattern$LastNode.match` | 1890 | 4.9% | 208 B | spill/frame-slot 699; reg move (arg shuffle/copy) 359; compare/branch (control) 189; frame-clear 113 |
| `ArrayList.grow` | 853 | 2.2% | 656 B | spill/frame-slot 264; heap load/store 196; compare/branch (control) 111; frame-addr (helper window) 60 |
| `Pattern.matcher` | 718 | 1.9% | 672 B | compare/branch (control) 258; allocation (TLAB bump/header) 86; spill/frame-slot 72; layout guard (hdr bit) 65 |
| `Arrays.copyOf` | 393 | 1.0% | 808 B | heap load/store 114; spill/frame-slot 110; compare/branch (control) 67; allocation (TLAB bump/header) 24 |
| `[unknown]` | 272 | 0.7% | - | runtime (Rust) |
| `<cratonvm_gc::zgc::ZgcRealHeap>::sweep_bitmap_range` | 184 | 0.5% | - | runtime (Rust) |
| `spin (driver + inlined row lambda)` | 163 | 0.4% | 608 B | frame-clear 96; compare/branch (control) 24; heap load/store 8; spill/frame-slot 8 |
| `<cratonvm_gc::heap_bitmap::HeapBitmap>::retain_marked_within_plain` | 91 | 0.2% | - | runtime (Rust) |

Row totals by class (top-10 JIT symbols): compare/branch (control) 8109, spill/frame-slot 7292, frame-clear 5098, heap load/store 3597, reg move (arg shuffle/copy) 3495, compare on frame slot 1812, jmp (block layout) 1345, layout guard (hdr bit) 1152, bounds check 1076, global flag check (barrier) 932.

HotSpot 25 (G1, C2; everything inlined, so symbols are coarse): `long R24RegexSplitRows$$Lambda/0x000000001c041000.run` 9832, `StubRoutines` 1253, `boolean java.util.regex.Pattern$LastNode.match` 554, `[unknown]` 48.

## Top 3 levers per row

"Need" is CratonVM main/op minus 3x HotSpot. Each estimate assumes the lever removes a fixed fraction of its class:
- register homes: 60 % of frame-slot traffic;
- hoisting or proving the loop guards: 70 % of the guard classes;
- removing frame clears in prologues and at spliced-region entries: 85 % (the bimodal ceiling is 100 %);
- inlining the call chain: 50 % of call overhead.

| row | need | lever 1 | lever 2 | lever 3 | sum |
|---|---:|---|---|---|---:|
| regex-matches | 5.1k | register homes in `BmpCharPropertyGreedy` / `matcher` / `spin`: 2.7k (of 4.5k) | inline the `matcher` -> `match` chain into the driver (drops IC, stack guard, helper window, arg shuffles): 1.3k (of 2.6k) | frame clears (bimodal ceiling 1.6k; the `BmpCharPropertyGreedy` prologue alone is 1.2k): 1.4k | 5.4k |
| compile | 2.0k | frame clears in the 0.8-1.3 KB prologues of `compile` / `sequence` / `atom` / `expr` / `closure` / `curly`, all bimodal's scope: 0.57k (ceiling 0.67k) | register homes: 0.53k (of 0.88k) | inline the small parse helpers (`RemoveQEQuoting`, `escape`, `has`, `peek`) and drop `jit_checkcast` (0.1k): 0.4k | 1.5k; short 0.5k, which field reloads could cover (`heap load/store` 0.54k: `cursor`, `temp`, `patternLength` reread after each call) |
| matchonly | 6.0k | register homes in the `Start.match` scan loop: 5.3k (of 8.9k) | hoist or prove the per-char guards (compact bit 1.9k, global word 1.6k, bounds 0.8k, poll 0.7k): 3.6k | hoist the `CharacterDataLatin1.A` static read and its null check out of the loop: about 0.5k (counted under control / heap) | 9.4k |
| regex-groups | 5.4k | register homes: 3.3k (of 5.5k) | guards: 2.5k (of 3.7k) | splice `Matcher.group` / `substring` (3.6k, heap-load heavy: `groups[]` and `text` reread): 1.5k | 7.3k |
| regex-find-email | 9.6k | register homes: 5.1k (of 8.5k) | guards: 4.7k (global 1.9k, layout 1.5k, class tests 1.3k on the `Range` / `CharPredicate` lambdas, bounds 1.2k) | arg shuffles and IC calls into the predicate lambdas (`reg move` 4.3k): 2.2k | 12.0k |
| regex-find-ci | 5.0k | register homes: 4.6k (of 7.7k) | guards: 2.7k (of 3.9k) | per-character case folding (`Character.toUpperCase` / `toLowerCase` spliced; part of control's 10.4k): not measured on its own | 7.3k |
| split-ws | 7.6k | frame clears, the per-iteration block in `Pattern.split` plus prologues: 5.7k (ceiling 6.7k; bimodal's prologue scope covers about 1.2k of it) | register homes: 5.9k (of 9.9k) | guards: 3.3k (of 4.8k) | 14.9k |
| cached | 3.4k | frame clears (the `Pattern.split` per-iteration block is 4.5k): 4.3k (ceiling 5.1k) | register homes: 5.5k (of 9.1k) | guards: 3.2k (of 4.6k) | 13.0k |

Three levers are common to every row:
- **Keep spliced-body values in registers.** This is the biggest lever on all eight rows. It is a register-allocator / home-assignment question.
- **Stop clearing reference homes at the entry of a spliced region inside a loop.** It is cheap, and it closes `cached` alone.
- **Prove or hoist the per-access guards in counted scan loops:** the compact bit, the global word, bounds checks, polls.

The bimodal lane's prologue work has these ceilings:
- `regex-matches`: about 1.6k (10 %).
- `compile`: 0.67k (15 %).
- `split-ws` / `cached`: only about 1.2k, because most of their frame clears are the in-loop block.

## What would close it

Every row at or under 3x. Take the levers in the order of the table. After each lever, re-run `prof.sh` and `ana2.py` on the new binary to confirm it moved its class.
