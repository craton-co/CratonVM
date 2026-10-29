# rx27 guards: the per-character compact-layout field test and global word that remain

Status: OPEN. Area: JIT IR field access (`ir_lower` `proven_receiver_mode`, `emit_field_layout_guard`,
`emit_read_bounds_live_check`). Severity: perf. The w4 profile measures the layout guard at 0.8-1.9k and the global
flag check at 0.9-1.9k instr/op per row. Filed 2026-10-28 by round rx27 wave 4, lane guards, by reading; no code
changed.

## What the two tests are

- **`test byte [obj+7],4` + `je`.** This is the per-object `GC_FLAG_COMPACT` test before an inline field access.
  Round 25 item 4 (`CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS`, with `_SPLICED`, `_DOMINANCE`, `_COESTABLISH`,
  `_PREHEADER`, all default ON) replaces it with ONE deopting guard per receiver, which later sites reuse. It
  is still per character only at a site the proof refuses. The refusal reasons are on `proven_receiver_mode`:
  - no de-spec registry in scope;
  - a rejoining branch ahead of the guard;
  - the receiver is a new object each iteration;
  - the site may not trap.
- **The global word.** This is either the layout-epoch guard (`emit_field_layout_guard`: the epoch only grows) or
  the ZGC read-barrier table check (`emit_read_bounds_live_check`). The latter must stay per reference read,
  because "whether reference SLOTS hold plain pointers can change at any safepoint".

## Why it was not changed here

Both tests are GC soundness. The per-object compact bit is fixed at allocation and kept by every collector, so a
per-CLASS "no legacy object exists" answer would need a VM invariant that the GC owner maintains: a latch set the
first time an object of the class is laid out legacy. No such latch exists; adding one is a GC change. The
read-barrier check is ZGC's.

## What would close it

1. **Census first.** Run `CRATONVM_DBG_JITC=1 ... R24RegexSplitRows matchonly 1 3 2>&1 | grep 'ir proven-receiver-fields'`
   and read `full=` / `establish=` / `proven=` for `Start.match`, `Matcher.search` and the spliced
   `BmpCharPropertyGreedy.match`. A `full=` count left on a loop-invariant receiver (the subject String, the
   matcher) names a refused proof. The fix belongs in `proven_receiver_mode`'s refusal for that shape.
2. **The epoch guard.** If the census shows the epoch guard in a loop where the proof was established, hoist it:
   the epoch only grows, so a passed deopting guard holds for the activation.
3. **A per-class compactness latch** (GC owner): would let a compile-time "this class is always compact" answer
   drop the test outright.

## Wave 5 (2026-10-29, lane guards): what the global word is, and the per-site census

**The global word is the LAYOUT-REPLACE EPOCH, not a GC barrier word.** It is `ir_lower::emit_layout_epoch_cmp`:
`CMP dword [rip + LAYOUT_REPLACE_EPOCH], expected`, where `expected` is the epoch baked at compile start (0 on
these rows). The fallback form is `MOV ECX, [R11]; CMP ECX, imm`.

The invariant (doc of `emit_field_layout_guard`):
- An object's layout is fixed at allocation.
- `register_class_layout` bumps the epoch (Release) under the registry's write lock BEFORE it publishes a
  replacement layout.
- An allocation reads the layout through that lock.
- So a deopting epoch guard that passed after the receiver was defined proves, for good, that the baked compact
  displacement is right or that the object is legacy (which the per-object test still routes). The epoch only
  grows.
- No safepoint flips it: only a class-layout replacement does, and that already sends every deopting guard to the
  interpreter.

The per-object `GC_FLAG_COMPACT` bit is fixed at allocation. Every collector copies headers whole, and the G1
old-generation stamp and monitor inflation both keep it. So no safepoint flips it either.

The ZGC read-barrier word is a different test (`CMP qword [JIT_READ_BOUNDS+8],0`). It sits on reference LOADS only
and must stay per read: ZGC arms it at a safepoint.

**Consequence for step 3.** Both tests are invariant for the life of the object and of the activation (up to a
deopt). A hoist therefore needs no re-test after safepoints; it needs a receiver that is the same object on every
trip (loop-invariant), and a deopt point at the loop entry that can re-execute. That is the proven-receiver
machinery's `Establish` with a preheader placement (`CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS_PREHEADER`, default
ON). The census below says why it declined at the remaining sites.

**Per-site census (diagnostic only):** `CRATONVM_DBG_PROVEN_FIELD_SITES=1` prints one `proven-field-site FULL
<load|store>` line per site that keeps the full sequence. Each line has the receiver op, whether the receiver's
block dominates the site, `this`, whether the site is spliced, and the first refusal reason in
`proven_receiver_mode`'s own order: arm-shape, no-block, no-despec-registry, withdrawn, may-not-deopt,
self-locking, spliced-no-chain-trap, frame-not-located, pc-not-lone, layout-guard-rejoins. Counts requested from
the prof agent; step 3 waits on them.

## Wave 5b (2026-10-29, lane guards; b15 census, pending build): `CRATONVM_JIT_IR_PROVEN_FIELDS_HOIST` (opt-in)

**The census on b15** (prof agent), default flags:
- `Start.match` keeps the full sequence at 3 load sites, all `pc-not-lone`: 2 on parameter receivers and 1 on a
  spliced field load. All 3 receivers dominate the site.
- `Pattern.split` keeps it at 22 store sites on objects it allocates (`recv=New`), all `may-not-deopt`.
- Executed cost, from samples on the compact test, the epoch compare and their jumps:
  - `matchonly`: about 1.26-1.35k instr/op (4.7 %).
  - `split-ws`: about 1.66k instr/op (3.7 %).

**Change (one opt-in switch, both items):**
1. `Lowerer::coestablish_param_proofs`. At an establishing site in a block on no cycle, every other reference
   PARAMETER that has a field site in a block this one strictly dominates gets a null test and a compact test,
   both deopting at the same bci. This is the W6-E receiver co-test, extended to parameters. The preheader rule
   then lets the proof cover the loop body. A legitimately null or legacy parameter deopts there, and the
   de-spec registry withdraws the pc.
2. `Lowerer::new_compact_proof_pc`. Right after an `Op::New`, one compact test on the fresh object, deopting at
   the `new`'s pc. Conditions:
   - a de-spec registry is in scope and has not withdrawn the pc;
   - the class is finalizer-free (`Graph::finalizer_free_new_classes`), so the interpreter's re-executed `new`
     registers nothing twice;
   - the site may trap (`field_site_may_trap`; `Op::New` is a predicted deopt point);
   - some field load or store takes the object as its receiver.

   The test runs on both allocation paths. Making the TLAB path free would mean placing it on the slow path only
   (the helper is the one that can return a non-compact object); left for a follow-up if the census shows it
   matters.

Soundness: both facts are fixed for the object's life and survive every collector and safepoint (wave 5 above),
so a proof put down once holds for the activation.

Engagement (`CRATONVM_DBG_JITC=1 CRATONVM_JIT_IR_PROVEN_FIELDS_HOIST=1 CRATONVM_DBG_PROVEN_FIELD_SITES=1`):
`grep -c 'proven-fields-hoist param'`, `grep -c 'proven-fields-hoist new'`, and the FULL-site census per body
(want Start.match 3 -> 1, the spliced load; Pattern.split 22 -> fewer).

Tests:
- `rx27_the_hoist_co_establishes_a_parameter_beside_another_values_read`, executed: on, p.b drops its own test and
  epoch guard; a legacy p leaves at the co-test's pc 1; off, it leaves at pc 10.
- `rx27_the_fresh_object_proof_keeps_its_rules`: source pins plus the opt-in default.

Arms: `icount2.sh rx27h matchonly,split-ws,cached,regex-split,regex-groups hs:hs: def:<tag>:
hoist:<tag>:CRATONVM_JIT_IR_PROVEN_FIELDS_HOIST=1`. Facts and the GC battery under `hoist`, in both modes.

## Close-out (2026-10-29, lane guards): every guards switch removed; the lever is closed for round rx27

All measured on the Linux host by the prof agent: icount2 main/op, median of 5, with facts and the GC battery in both
modes.

| switch | what it did | engaged | result | fate |
|---|---|---|---|---|
| `CRATONVM_JIT_TRUST_BOOT_PRIM_ARRAY_FINALS` | boot-class static-final primitive tables trusted: length filed, read hoisted | 15 lengths filed, 5-6 reads hoisted (b7-b9) | b8 `tbl` and b9 `tbl` inside noise (find-email -1.6 %, overlapping) | removed (`5ea831d133`) |
| `CRATONVM_JIT_IR_BCE_CONST_LENGTH_RANGE` | bounds check dropped for a compile-time-length array, with the `(x & M) == 0` edge and Guard facts | 7 `ctype` checks proved per matchonly compile (b9) | b9 `bce`: cached -7.4 % but overlapping; the rest noise | removed (`5ea831d133`) |
| `CRATONVM_JIT_IR_POLL_TRUE_BACK_EDGES` | no poll on a forward CFG edge laid out backwards | never: the two polls per character belong to two real loops | no effect | removed (`5ea831d133`) |
| `CRATONVM_JIT_IR_PROVEN_FIELDS_HOIST` | item 1: parameter compact proof at the loop-entry read; item 2: proof after `new` | item 1: `Start.match` full-test loads 3 to 1 (b16); item 2: 17-19 parse-body `new`s, none in `Pattern.split` (b17: spliced `new`s declined `class-not-proven-finalizer-free`) | b16: matchonly -0.7 %, no row disjoint | removed (`ddbbf53f16`) |

Why the levers do not pay:
- The full compact test plus the epoch compare cost about 1.3k instr/op on matchonly (4.7 %) and 1.7k on split-ws
  (3.7 %) on b15. Item 1 removed most of matchonly's share, and the row did not move beyond noise.
- `Pattern.split`'s full-test stores depend on compile shape: 22 on b15, 9-11 on b16, 2 on b17. So item 2 had little
  to win.
- The proved bounds checks and the hoisted table read are a few instructions per character each, below this host's
  noise.

What is kept:
- The per-site census `CRATONVM_DBG_PROVEN_FIELD_SITES` (`3f50ad93d9`, diagnostic only).
- The typecheck census reason `lambda-proxy`.

What would reopen this page:
- A lever that stacks these levers with the register-home and frame-clear work, so their few percent become
  measurable.
- Or a per-class compactness latch (GC owner) that removes the test outright from every site.

Status stays OPEN: the per-object test still runs at the sites the census names. The guards lane has no work left
on it this round.
