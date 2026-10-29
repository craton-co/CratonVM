# STR16-P3: what the String-intrinsic pin still pins, and the A/B that lifts it

> **j27 wave 8 (lane W8LOOP, 2026-10-28): NOT FIXED (nothing flipped), PENDING HOST BISECT.** No code is needed
> for the next step. The members wave 5 suspects each already have a kill word inside
> `CRATONVM_JIT_IR_SB_PREFIX` (`jit/src/lib.rs`):
> * `CRATONVM_JIT_IR_SB_APPEND_INT_PREFIX`
> * `CRATONVM_JIT_IR_SB_TOSTRING_PREFIX`
> * `CRATONVM_JIT_IR_SB_APPEND_STRING_PREFIX`
> * `CRATONVM_JIT_IR_SB_CHARAT_STAYS_CALL`
>
> All four are default ON. Each one's `=0` restores that site's pin. **Bisect recipe** (one binary, P-cores, 5
> interleaved reps, `--jdk-only`, rows `S17Kernel chunk` / `osr`, `Nio20StringIr sbappend`, `J27StrpinRows sb`):
> * D = `CRATONVM_JIT_IR_SB_PREFIX=1`.
> * D1 = D + `CRATONVM_JIT_IR_SB_APPEND_INT_PREFIX=0`.
> * D2 = D + `CRATONVM_JIT_IR_SB_TOSTRING_PREFIX=0`.
> * D3 = D + `CRATONVM_JIT_IR_SB_APPEND_STRING_PREFIX=0`.
>
> Also record the `pinned=` census (`CRATONVM_DBG=jit-method-stats`) per arm, and require facts equal on every arm.
> **Rule.** Find the single arm that brings `S17Kernel chunk` / `osr` back to A's 86 / 94 while keeping D's
> `sbappend` / `sb` wins. That member is the loser. Then the decision is: flip `CRATONVM_JIT_IR_SB_PREFIX` ON with
> that member's kill as its new default, in a `WARNING:` commit. If no single arm does it, keep the pin as wave 5
> decided.

> **j27 wave 5 (lane STRPIN, 2026-10-28): section 3's pin A/B MEASURED (Windows, local build), decision: KEEP THE PIN,
> `CRATONVM_JIT_IR_SB_PREFIX` stays opt-in. NOT FIXED (nothing flipped), PENDING HOST CONFIRMATION.**
> One binary, P-cores, 5 interleaved reps, `--jdk-only`, median ns/op (A default, B `CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1`,
> C = B + `CRATONVM_JIT_IR_SB_PREFIX=1`, D `CRATONVM_JIT_IR_SB_PREFIX=1`; HotSpot 25 in the last column):
>
> | row | A | B | C | D | HotSpot |
> |---|---:|---:|---:|---:|---:|
> | `S17Kernel chunk` | 86 | 204 | 108 | 100 | 52 |
> | `S17Kernel osr` | 94 | 201 | 108 | 106 | 44 |
> | `Nio20StringIr sbappend` | 55 | 107 | 44 | 43 | 17 |
> | `Nio20StringIr sbappendstr` | 39 | 39 | 39 | 37 | 23 |
> | `J27SmallCallRows sb` | 45.7 | 81.1 | 35.0 | 34.9 | 3.4 |
> | `J27StrpinRows sb` | 46.5 | 84.0 | 40.4 | 40.2 | 4.6 |
> | `J27StrpinRows pinned-setters` | 62.1 | 60.5 | 62.0 | 11.7 | 1.2 |
> | `J27StrpinRows pinned-dec` | 182 | 178 | 171 | 183 | 8.0 |
> | `DecLoop sb` / `dec` | 65 / 160 | 70 / 176 | 60 / 177 | 58 / 150 | 5 / 9.5 |
>
> Census (`CRATONVM_DBG=jit-method-stats`): A `pinned=3` (method entry) + 1 (OSR) on `S17Kernel chunk`, 1 + 1 on
> `sbappend`; B and D `pinned=0`, so every arm compares what it should. Reading: B (pin off, builder sites as calls)
> loses 2x on every builder row, so the pin is earning its keep. D wins the m7 row that was the one recorded loss
> (`sbappend` 55 -> 43; plausibly STR20-1's `SB_CHARAT_STAYS_CALL`, not bisected) and the short `sb` rows, and releases a
> pinned NIO method (`pinned-setters` 62 -> 12), but **loses `S17Kernel chunk` / `osr` by 15-20 %** (86 -> 100,
> 94 -> 106, every rep), which is section 3's "A wins an sb row by more than noise" outcome. Next suspect: the
> `append(int)` / `toString()` prefix bodies on the `S17Kernel` four-append chain (STR20-1's remaining suspects),
> not the String members. Facts (`Nio20StringIr conf`, `J27StrpinRows`, `J27SmallCallRows`) identical to HotSpot on
> all four arms, `--jdk-only`, `--compatible` and `--nojit`. `R21StrintrRows` / `R16Bench` were not on this machine
> (`C:\craton\jr21-ssh\probes` is gone); `J27StrpinRows sb` / `pinned-*` stand in for `sb-chunk`. The round's other
> change on this machine (`CRATONVM_JIT_IR_SCHED_NAMED_BEFORE_ANCHOR`, lane STRPIN) moves none of these rows.

> **j27 wave 1 (lane STRRX, 2026-10-28): the two switches are REVIEWED CORRECT and already default ON in code;
> the pin A/B (section 3) is NOT FIXED.** Doc/code drift: the t1 note below says "re-landed opt-in", but
> `jit/src/lib.rs` `ir_string_compare_to_mixed_enabled` / `ir_string_indexof_supplementary_enabled` read
> `runtime_flag_default_on` and the registry rows say default-on, kill `0` (since the re-land `44f0382076`).
> Code review against the JDK 25 sources (`src.zip` of 25.0.3): the mixed `compareTo` arm
> (`Lowerer::emit_str_compare_to_mixed_arm`) is `StringLatin1.compareToUTF16Values` /
> `StringUTF16.compareToLatin1` exactly (decoded unsigned chars, `len1 - len2` in chars, odd UTF16 length
> declines); every instruction encoding re-checked. The `indexOf(int)` UTF16 arm is `StringUTF16.indexOf`:
> `ch < 0x10000` (negatives included) scans code units (a negative never matches), else
> `indexOfSupplementary` (invalid -> -1, the pair as one little-endian dword at `i < len - 1`). No divergence
> found, so the decision is **keep both ON**; nothing to flip. The probe `tools/probes/t1/T1W8RlStrMixedCoder.java`
> (and its `=0` arm) is the host check. Also reviewed in the same pass, no divergence: `codePointAt` /
> `codePointBefore` / `codePointCount` (shared tails in `x64/string_code_point.rs`), `compareToIgnoreCase`'s
> Latin-1 fold (`0xB5` -> `0x3BC`, `0xFF` via `0x178`), `coder()` / `isLatin1()`, the accessor kinds.

> **t1 status (2026-10-26, wave 8, lane RL-STR):** the two switches' code was dropped at merge `624fae0b49`.
> RE-LANDED IN CODE as **opt-in** (`CRATONVM_JIT_IR_STRING_COMPARE_TO_MIXED=1`,
> `CRATONVM_JIT_IR_STRING_INDEXOF_SUPPLEMENTARY=1`; round 21 never verified them). PENDING HOST VERIFICATION:
> `tools/probes/t1/T1W8RlStrMixedCoder.java` facts = HotSpot 25 with both `=1` and unset, both modes; the pin A/B
> below is still open.

Status: PENDING HOST MEASUREMENT (code is in; the pin A/B below decides lifting `string-intrinsic-pin`; the two new switches are FIXED IN CODE (JIT round 21, lane strintr), PENDING HOST VERIFICATION)
Status: PENDING HOST MEASUREMENT (the pin A/B of section 3 decides lifting `string-intrinsic-pin` = STRINTR21-2; the wave-19 data and the rows still missing are in section 4.3); lane strintr's two switches FIXED IN CODE (JIT round 21, lane strintr), PENDING HOST VERIFICATION; STRINTR21-1 (section 4.1) FIXED IN CODE (JIT round 21, lane strintr2), PENDING HOST VERIFICATION
Area: JIT optimizing tier, `java/lang/String` intrinsics (`jit/src/lib.rs` pin + planning gate, `jit/src/ir.rs` accessor expander, `jit/src/ir_lower.rs` String search prefix)
Found by: JIT round 21 lane strintr (item STR16-P3, top of `jr21-r18-backlog-retriage-20261015.md` 1.1 / 2.1); base 2a0ec6cbb6

Re-land (20261026): merge `624fae0b49` had dropped this round-21 code from dev; it is back on dev since `44f0382076` (formatted 3-way re-land, `docs/internal/jit/jr21-merge-624fae0b49-dropped-round-21-code-20261026.md`). The status above describes the code again; its host verification is recorded there.

## 1. The row's premise no longer holds on this base

The backlog row says "the optimizing (IR) tier has no String access intrinsics, so `string-intrinsic-pin` keeps
String-heavy methods on the single-pass tier". Read against 2a0ec6cbb6, the optimizing tier answers **every**
`java/lang/String` / `java/lang/CharSequence` member that the single-pass region intrinsifies
(`try_resolve_string_intrinsic`'s STRING_ACCESS and STRING_SEARCH rows):

| member | single-pass region | optimizing tier on this base | switch (default) |
|---|---|---|---|
| `String.length()` / `isEmpty()` / `charAt(int)` | inline decode, deopt on a miss | `IrBuilder::try_string_access_intrinsic` (`ir.rs`): ordinary IR nodes, branchless LATIN1/UTF16 decode, LICM-hoistable `coder` / `value` / `length` loads; null receiver, null `value`, index out of range = `Op::Guard` deopt at the invoke bci (the interpreter re-raises the exact NPE / SIOOBE) | `CRATONVM_JIT_IR_STRING_INTRINSICS` (ON) |
| `CharSequence.length()` / `isEmpty()` / `charAt(int)` | guarded decode (needs a receiver profile) | `ir_lower` String search prefix kinds `SeqLength` / `SeqIsEmpty` / `SeqCharAt`: kind bits + exact String class id in machine code, decline to the interface call on a miss | `CRATONVM_JIT_IR_CHARSEQ_PREFIX` (ON) |
| `hashCode()` | cached field, else compute | prefix `HashCode`: cached non-zero `hash`; uncached COMPACT receiver computed inline and cached; `hashIsZero` honoured and set | `..._STRING_HASH_COMPUTE`, `CRATONVM_JIT_STRING_HASH_IS_ZERO` (ON) |
| `equals(Object)` | every coder pair | prefix `Equals`: identity, null, exact String, equal coders = byte compare (SSE2); unequal coders decline (deliberately, see the arm) | `..._STRING_SEARCH_PREFIX`, `..._STRING_SSE2` (ON) |
| `compareTo(String)` | every coder pair | prefix `CompareTo`: LATIN1/LATIN1, UTF16/UTF16; **mixed coders declined before this lane** | ON; mixed: NEW, below |
| `indexOf(int)` | constant BMP needle only | prefix `IndexOfChar`: LATIN1 any needle; UTF16 BMP needle (SSE2); **a UTF16 receiver with a negative / supplementary / invalid needle declined before this lane** | ON; non-BMP: NEW, below |
| `indexOf(String)` | every coder pair | prefix `IndexOfStr`: LL, UU, UL; LATIN1 hay + UTF16 needle declines | ON |

and the pin and the planning gate were already taught to step aside for exactly those sites
(`every_string_intrinsic_site_ir_expands`, `ir_string_search_prefix_handles`,
`string_site_bails_ir_planning`), nio20 waves 1-6. The pin's verdict for a method whose String-intrinsic sites
are all String / CharSequence members is `IrExpandsEverySite`: **not pinned**.

### What the pin still pins (by reading `string_intrinsic_pin_verdict`)

1. **Any method with a `StringBuilder` intrinsic site** -- `length()`, `charAt(int)`, `append(char)`,
   `append(String)`, `append(int)`, `toString()` (`try_resolve_string_intrinsic`'s STRINGBUILDER_ACCESS rows).
   The IR prefix for those (`SbLength` .. `SbToString`) exists but sits behind `CRATONVM_JIT_IR_SB_PREFIX`,
   which is **opt-in** since the m7 host pass (`Nio20StringIr sbappend` 64 vs 44 ns/op). Its round-20 fix
   (`CRATONVM_JIT_IR_SB_CHARAT_STAYS_CALL`, ON) is still PENDING HOST A/B
   (`jit-r20-strings-proposals.md` STR20-1). Exception: a method that also holds a
   `StringBuilder.appendCodePoint` site (`CRATONVM_JIT_IR_SB_CODE_POINT_UNPIN`, ON).
2. **Everything under narrow oops** (`x64::narrow_oops_block_inline_fields`): the search prefix refuses, so
   every non-accessor String site pins. Compressed oops are off by default.
3. Any site whose own switch is `=0` (the arms of the switches above).

So the pin now governs the `StringBuilder` population, and the `S17Kernel` / `R16Bench strings` /
`R21StrintrRows sb-chunk` kernels are exactly that population. The str17 A/B (`S17Kernel chunk`, 10.4 vs 10.8
CPU s, noise) was taken with the SB prefix unset and is the B arm below, not the C arm.

## 2. Round 21 fix (lane strintr): the two coder gaps left in the IR String prefix

Both are holes where an IR body (which the pin no longer prevents for String-only methods) paid a full JDK call
that the single-pass region answers inline. Both are performance changes behind NEW default-ON switches
(`types/src/flag_groups.rs`, `types/tests/flag-surface.txt`); `=0` restores the decline byte for byte.

* **`CRATONVM_JIT_IR_STRING_COMPARE_TO_MIXED`** (`jit/src/lib.rs::ir_string_compare_to_mixed_enabled`;
  `jit/src/ir_lower.rs` `try_emit_ir_string_search_prefix` `K::CompareTo` arm + new
  `Lowerer::emit_str_compare_to_mixed_arm`). A LATIN1/UTF16 pair (either direction) is compared over decoded
  chars: `this[k] - that[k]` at the first differing `k < min(len1, len2)`, else `len1 - len2` in chars --
  `StringLatin1.compareToUTF16` / `StringUTF16.compareToLatin1` (`= -compareToUTF16(other, this)`) exactly.
  Right in both modes: the `--compatible` native compares decoded chars too, and no compact-strings invariant is
  assumed (unlike a mixed `equals` / `indexOf(String)`, which is why those two stay declined). An odd UTF16 byte
  length (not a String the JDK builds) and every earlier decline (null argument, non-String class id, null
  `value`) still go to the call. Scratch RAX/RCX(read)/RDX/R10/R11(read) only; no call, no GC point, no deopt.
* **`CRATONVM_JIT_IR_STRING_INDEXOF_SUPPLEMENTARY`** (`ir_string_indexof_supplementary_enabled`; the
  `K::IndexOfChar` UTF16 arm). A UTF16 receiver with a needle outside `0..=0xFFFF`: a negative or `> 0x10FFFF`
  needle answers `-1` (`StringUTF16.indexOfChar` never matches a negative `int`; `indexOfSupplementary` gates on
  `Character.isValidCodePoint`), a supplementary one scans for its surrogate pair as one little-endian dword
  (`hi | lo << 16`, `hi = (ch >>> 10) + 0xD7C0`, `lo = (ch & 0x3FF) + 0xDC00`) at char `i < length - 1`. A lone
  high surrogate at the end, and the pair split by the string end, answer -1 as the JDK loop does.

Exceptions: neither arm raises anything; every exception case (null receiver / argument) still declines to the
call, which raises it, so messages are the JDK body's (`--jdk-only`) or the native's (`--compatible`), as before.

Unit tests (executed bodies over fake Strings): `jit/src/ir_lower.rs`
`nio20_ir_string_search_prefix_answers_inline_or_dispatches` now asserts, with the switches on, `indexOf` of
U+10437 / U+1F600 / U+10FFFF pairs at 0, 1, 3, -1 (lone / split surrogates), `-1` / `0x110000` /
`Integer.MIN_VALUE` = -1, and mixed `compareTo` both directions over six pairs (prefix-equal, longer/shorter,
`0xFF` vs `0x100`, empty, `0xFFFF`, 40-char) against a decoded-char model; the old MARKER (call) asserts stay as
the `=0` arm.

### HOW TO VERIFY

Probe `C:\craton\jr21-ssh\probes\R21StrintrRows.java`, both modes:

* `fact` lines (charAt / length / isEmpty, SIOOBE messages for LATIN1 / UTF16 / negative / empty /
  `Integer.MIN_VALUE` index, the NPE messages, mixed and same-coder `compareTo`, supplementary / invalid
  `indexOf`, `indexOf(String)`, `equals`, `hashCode` incl. `""` and `"\0"`) must equal HotSpot 25 on every arm.
* `cmp-mixed`: default vs `CRATONVM_JIT_IR_STRING_COMPARE_TO_MIXED=0` -- expect default faster (no JDK call
  per compare). Engagement: `CRATONVM_DBG_IR_COMPILES=1` prints `[ir] string-search-prefix n.. java/lang/String.compareTo(..) kind=CompareTo`
  for `cmpMixed`.
* `indexof-supp`, `indexof-neg`: default vs `CRATONVM_JIT_IR_STRING_INDEXOF_SUPPLEMENTARY=0` -- expect default
  faster.
* The other `time` rows are reference readings and must not move between those arms.

## 3. The A/B that decides lifting the pin (not flipped by this lane)

Arms (one binary, both modes, each row in its own process, CPU time -- `/usr/bin/time` user+sys -- on the loaded
host, interleaved, >= 5 repeats):

| arm | env |
|---|---|
| A | default (pin ON, `CRATONVM_JIT_IR_SB_PREFIX` unset) |
| B | `CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1` |
| C | `CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1 CRATONVM_JIT_IR_SB_PREFIX=1` |
| D | `CRATONVM_JIT_IR_SB_PREFIX=1` (pin ON; with the SB prefix every builder site is a prefix kind, so the pin releases those methods by itself -- D and C should read alike) |

Rows: `tools/probes/S17Kernel chunk` and `osr`; `R16Bench strings`; `tools/probes/Nio20StringIr
-Dnio20.rows=sbappend,sbappendstr`; `R21StrintrRows -Dr21.rows=sb-chunk`, `scan-escape`, and (control, must
not move) `charat-l1`, `cmp-l1`. Census on every arm: `CRATONVM_DBG=jit-method-stats` ->
`JIT String-intrinsic pin by door: ... pinned=N` (A: N > 0 on the sb rows; B/C/D: 0) -- a zero on A means the
row never reached the pin and the arm compares nothing.

Decision:

* **C (or D) <= A on every row within noise, both modes** -> flip `CRATONVM_JIT_IR_SB_PREFIX` to default ON (that
  alone releases the builder methods: arm D), and then the pin has no population left except narrow oops; retire
  the pin, `ir_over_intrinsic_enabled`, `ir_string_access_expander_handles` and the fail-closed rule together
  (their doc comments stage that), with narrow oops keeping the planning-gate refusal.
* **B <= A everywhere but C loses** -> lift the pin only (the SB sites become ordinary spliced / called JDK
  bodies), keep the SB prefix opt-in; re-read STR20-1 first.
* **A wins any sb row by more than noise** -> keep the pin; the residual is the SB prefix's body (STR20-1's next
  suspects: per-call receiver re-read + epoch guard per prefix, the decline-to-call on growth), not the String
  members, which this page shows are all covered.

TP16-13 / TP16-16 (`jr21-r18-backlog-retriage-20261015.md` 2.2) are no longer "blocked on the pin": a method
whose only String sites are accessors is already an IR body on this base, so the accessor expander's null /
out-of-range deopt is live today. `R21StrintrRows charat-oob` is the row that prices it (each SIOOBE in the IR body
is a deopt at the invoke, then the interpreter raises it); proposal STRINTR21-1.

## 4. Round 21 wave 19 (lane strintr2): STRINTR21-1 built, the charAt rows read, STRINTR21-2's data

Base 5906cd86a4. Host numbers the wave started from (`R21StrintrRows`, `--jdk-only`, median ns per op):
charat-l1 154 / HotSpot 8, charat-u16 332 / 10, charat-oob 6993 / 417, sb-chunk 552 / 17, scan-escape 157 / 87.

### 4.1 Round 21 fix (lane strintr2): STRINTR21-1, a trapped accessor site becomes a declining call

**What was wrong.** `IrBuilder::try_string_access_intrinsic` builds three kinds of `Op::Guard { bci: pc }` (null
receiver, null `value`, index out of range). A failing one is a `DeoptReason::UncommonTrap` deopt, and the VM
sink (`helpers::DeoptimizationController::deoptimize_in`) records it AND evicts the artifact; after
`PER_BCI_DESPEC_LIMIT` (4) charges at the bci it also records the site in the de-spec registry. The expander never
asked that registry (unlike `tl_trap_site_admits`, the never-taken prune or the class predicate), so every
recompile re-expanded the site and the next out-of-range index deopted and evicted again, until the per-method
escalation gave the method up. `charAtOob` (one SIOOBE per two calls, caught in the same method) therefore ran its
loop through deopt + eviction + recompile churn and then interpreted: 6993 ns/op, ~14 us per throw against
HotSpot's ~830.

**What changed** (`jit/src/lib.rs`, `jit/src/ir.rs`, `jit/src/ir_lower.rs`; perf, both behind NEW default-ON rows):

* `CRATONVM_JIT_IR_STRING_ACCESS_TRAP_WITHDRAW` (`lib.rs::ir_string_access_trap_withdraw_enabled`,
  `string_access_withdrawn_pcs`; `ir_tier_attempt` -> `IrBuilder::set_string_access_withdrawn_pcs`): the method's
  `invokevirtual` pcs whose `(UncommonTrap, pc)` speculation (or the wildcard) the registry holds under
  `"class.name:descriptor"` (the key the trap sinks charge). `try_string_access_intrinsic` refuses such a site
  (after the splice / guarded refusals, before anything is consumed), so it is built as its ordinary `Op::Call`.
  Census row `trap_withdrawn` (`CRATONVM_DBG_IR_STRING=1`); `CRATONVM_DBG_IR_COMPILES=1` prints
  `[ir] string-access-withdrawn pc=.. java/lang/String.charAt(I)C: kept as a call`.
* `CRATONVM_JIT_IR_STRING_ACCESSOR_CALL_PREFIX` (`ir_string_accessor_call_prefix_enabled`): three new
  `IrStringSearchKind`s, `StrLength` / `StrIsEmpty` / `StrCharAt`, named by `ir_string_search_prefix_kind` for the
  `java/lang/String` accessors. `try_emit_ir_string_search_prefix` lowers them with the `SeqLength` / `SeqIsEmpty`
  / `SeqCharAt` arms minus the kind-bit test (the exact-String class-id compare stays): a hit is one coder test, one
  unsigned bounds compare and one load. A null receiver, a null `value`, an index outside `[0, length)` and a
  layout epoch change DECLINE to the unchanged call, which raises the exact NPE / SIOOBE (the JDK body in
  `--jdk-only`, the native in `--compatible`, as before). Never the trapping form (`trap` stays `SeqCharAt`'s).
  The kinds are filtered out of `ir_string_search_prefix_handled_kind`, so the pin, the planning gate,
  `splice_loses_string_intrinsic` and both splice carve-outs decide exactly as with `=0`; the kinds only front an
  `Op::Call` the expander left behind: a withdrawn site, and a String accessor call inside a splice
  (`SI_IN_SPLICE`, a bare call until now). `call_site_may_take_inline_prefix` asks `ir_string_search_prefix_kind`
  directly, so its superset covers them.

**Why it is right.** No new speculation: a withdrawn site plants no guard, and the prefix never deopts. The
exception paths are the call's, i.e. what the interpreter's re-execution produced before (same callee, same
message, both modes). The withdrawal is per bci and per method key, asked at the grain the sink records
(`contains_speculation(key, bci, speculation_id(bci, UncommonTrap))`, the wildcard included).

**What it does not do.** With `CRATONVM_JIT_IR_CALL_SITE_EXCEPTION_EDGES` off (its default) a throw out of a call
whose handler is in the same IR body still leaves the activation (a reason-9 rethrow exit to the interpreter, not
an eviction), and the loop then OSRs back. So charat-oob should drop from "deopt + evict + recompile per throw" to
"one rethrow exit per throw", not to HotSpot's in-frame catch. Arm D below prices the rest (STRINTR21-6).

**Unit tests.** `jit/src/lib.rs` `strintr2_string_accessor_call_tests`: the withdrawn set (named and wildcard
entries, another key, no registry, the kill switch, an `invokestatic` pc never listed); the kinds (named under the
switch, `None` with `=0`, never "handled", `SeqCharAt` untouched, opcodes / invoke kinds / `num_params`, not kept a
call by the splice planners).

### HOW TO VERIFY (STRINTR21-1)

Probe `C:\craton\jr21-ssh\probes\R21Strintr2Rows.java`, both modes, rows `oob-half`, `oob-rare`, `npe-half` and
`charat-hot` (control), plus `R21StrintrRows -Dr21.rows=charat-oob`:

| arm | env | expect |
|---|---|---|
| A | default | oob-half / npe-half / charat-oob far below B (no eviction churn) |
| B | `CRATONVM_JIT_IR_STRING_ACCESS_TRAP_WITHDRAW=0` | today's numbers (charat-oob ~7000) |
| C | `CRATONVM_JIT_IR_STRING_ACCESSOR_CALL_PREFIX=0` | ~A on the *-half rows; above A on oob-rare (63 of 64 calls are prefix hits) |
| D | `CRATONVM_JIT_IR_CALL_SITE_EXCEPTION_EDGES=1` (opt-in, not this lane's) | the in-frame catch: how much of A is the rethrow exit |

`charat-hot` and `R21StrintrRows charat-l1` / `charat-u16` must not move between A, B and C (nothing traps,
nothing is withdrawn). Every `fact` line must equal HotSpot 25 on every arm (`oob-last`, `oob-l1`, `oob-u16`,
`oob-empty` carry the SIOOBE texts read from the withdrawn, rebuilt site). Engagement on A:
`CRATONVM_DBG_IR_STRING=1` -> `trap_withdrawn=N`, N > 0 (0 on B); `CRATONVM_DBG_IR_COMPILES=1` ->
`string-access-withdrawn` then `string-search-prefix .. java/lang/String.charAt(I)C kind=StrCharAt`;
`CRATONVM_DBG_DEOPT=1` -> the `per-bci de-spec` line once, then no more deopts at the `charAt` bci.

### 4.2 The charAt rows are not a call per char

`R21StrintrRows` prints `best / perCall`, and `perCall` counts OUTER iterations; each one walks a whole string.
`L1` words are `12 + nextInt(40)` chars (mean 31.5), `U16` words `len + 1 + len / 2` (mean ~48). So charat-l1's
154 ns is ~4.9 ns per char and charat-u16's 332 ns ~6.9 ns per char; HotSpot's 8 / 10 are 0.25 / 0.2 ns per char
(an unrolled, vectorised loop). The IR expansion is engaged: a call per char costs far more (the 20260902 page's
arm table: ~76 ns per char for the dispatched shape). What is left is the per-node cost of the expansion in a
lowerer that homes every value in a frame slot: per char, after `licm-read-hoist` has lifted `coder` / `value` /
`value.length`, the body still runs the receiver and `value` null guards of BOTH the loop condition's `length()`
and the body's `charAt`, two bounds guards, `index << coder`, two `ArrayLoad(Byte)`s (each with its own null and
bounds check unless `ir_check_elim` proves it) and the six-node branchless combine. That is the general codegen gap
`string-charat-loop-cost-and-the-unsteerable-intrinsic-FIXED-20260902.md` left open ("the `char[]` control in the
same run reads ~1.0 against HotSpot's ~0.3"), not a missing intrinsic. Filed, with the two fix shapes, as
`jr21-strintr2-charat-loop-per-char-node-cost-20261014.md` (STRINTR21-5).

### 4.3 STRINTR21-2: the data the wave has, and what is still missing

The host's pin A/B (`out8.log`, one binary, `--jdk-only`), arms A default, B pin off, C pin off + SB prefix,
D SB prefix only:

| row | A | B | C | D |
|---|---:|---:|---:|---:|
| charat-l1 | 159 | 183 | 162 | 155 |
| charat-u16 | 331 | 520 | 333 | 331 |
| charat-oob | 6917 | 8093 | 6913 | 6766 |
| cmp-l1 / cmp-mixed | 4 / 8 | 7 / 8 | 4 / 8 | 4 / 7 |
| equals-l1 / hash-cached | 3 / 3 | 5 / 5 | 3 / 3 | 3 / 3 |
| indexof-supp / indexof-neg / indexofc-l1 | 27 / 2 / 4 | 46 / 4 / 7 | 27 / 2 / 4 | 19 / 2 / 4 |

* D <= A on every row shown, and C ~ A: the SB prefix costs the String rows nothing.
* **The rows that decide are missing.** The printed table stops at `indexofc-l1` (alphabetical), so `sb-chunk`,
  `scan-escape` and `indexofc-u16` for the four arms were not in the log handed to this lane. Re-run exactly
  section 3's rows on arms A and D (both modes, with the `pinned=N` census) before flipping.
* **B moves rows the pin does not govern** (cmp-l1 4 -> 7, charat-u16 331 -> 520, indexof-supp 27 -> 46). Those
  methods hold no builder site, so `string_intrinsic_pin_verdict` answers `IrExpandsEverySite` on both arms.
  Either host noise (HotSpot's own charat-l1 read 8 in one run and 27 in the next) or a population this page has
  not named (a JDK helper with a builder site, compiled differently, running inside the timed call). Read
  `CRATONVM_DBG_JITC=1` for `R21StrintrRows.cmpL1` on A and B before quoting B; it does not change the D-vs-A
  decision.
* m7's `Nio20StringIr sbappend` (64 vs 44 ns/op with the SB prefix, both modes) is the one measured loss on record,
  and STR20-1's `CRATONVM_JIT_IR_SB_CHARAT_STAYS_CALL` (ON since) its suspected cause. Include `sbappend` /
  `sbappendstr` in the D-vs-A run: if D still loses there, the flip waits for STR20-1's next suspect.
* sb-chunk itself (552 vs HotSpot 17) is not a pin question on either arm: HotSpot removes the builder
  (`OptimizeStringConcat` + escape analysis); both CratonVM tiers build it, call `<init>` and append in place.
  Filed as `jr21-strintr2-sb-chunk-builder-chain-is-not-fused-FIXED-20261027.md` (STRINTR21-4).
