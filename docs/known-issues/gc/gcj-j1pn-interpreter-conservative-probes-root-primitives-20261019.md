# The interpreter's conservative probes treat primitive `long` / `double` slots as references

> **STATUS (2026-10-19, gcj j2/pn2): ITEM 2 FIXED IN CODE under the existing opt-in switch; ITEM 4 FIXED IN CODE under a new opt-in switch; item 5 (doc) fixed; item 3 open; PENDING BUILD/PROBE.** Item 2: `vm/src/runtime/value_stack.rs` `ValueStack::scan_object_refs_conservative` honours `KIND_LONG` / `KIND_DOUBLE` (minted handles still root) under `CRATONVM_GEN_CONSERVATIVE_LOCALS_SKIP_PRIMITIVE_KINDS` on Generational (test `gcj2pn2_conservative_probe_skips_long_and_double_kind_slots`). Item 4 (and a stricter item 1): `CRATONVM_GEN_INTERP_LOCALS_TYPE_MAP` (opt-in, Generational only) -- the frame's reference-typed locals (`local_liveness::TypedMask`, the GenerateOopMap answer gci i10/np already computes) gate the conservative locals probe and the lost-tag arm of `Frame::update_local_refs`: a slot not typed a reference at the frame's pcs is neither probed nor rewritten by value (test `gcj2pn2_the_type_map_keeps_an_int_typed_local_out_of_the_conservative_probe`). This is stage 1 of `gcj-j1pn-proposal-precise-interpreter-frames-from-the-verifier-type-maps`. Item 5: `vm/src/memory/roots.rs` `conservative_locals_enabled` doc. Item 3 (`--compatible` minted-only rule) open: it changes `--compatible` retention on every frame scan and needs its own `--compatible` battery. Rows: blocks PL2 / TM of `docs/internal/gc-round-gcj-20261019/j2-pn2-report.md` (note: j1's PL loop rows were never extracted, so `pnj1_pl_off_*` / `pnj1_pl_on_*` have no j1a reading; PL2 restates them unrolled).**

> **STATUS (2026-10-19, gcj j1/pn): OPEN -- found by reading; item 1 has an OPT-IN fix in code (`CRATONVM_GEN_CONSERVATIVE_LOCALS_SKIP_PRIMITIVE_KINDS`, Generational only), the rest open.** Retention and over-pinning, never corruption by itself (a `long` / `double` kind slot is never rewritten, see item 4). HotSpot never keeps an object alive through a primitive.

1. **Locals (opt-in fix).** `vm/src/runtime/frame.rs` `Frame::scan_locals_conservative` probes
   every live local's object decode, `long` payload and raw bits with `is_object_address`, and did
   not consult `local_kinds`. The precise scan (`scan_locals_in`) already trusts the kind mark
   (`LKIND_LONG` / `LKIND_DOUBLE` iff the slot holds that primitive) and roots only a minted
   smuggled handle there. So a live `long` whose value equals a young object's address is a root
   whenever the conservative probe runs (a compiled frame or a conservative peer is live), and
   under selective promotion it pins that object young. Fix built opt-in: under the switch the
   probe honours the kind mark exactly as the precise scan does. Test
   `runtime::frame::tests::gcj1pn_conservative_probe_skips_a_long_kind_local_under_the_switch`;
   probe `tools/probes/Gcj1PnLongLocalProbe.java` (rows in `docs/internal/gc-round-gcj-20261019/j1-pn-report.md`).
   Flip when the probe's switch arm prints `PASS long-local` and every row of block PL is SAME.
2. **Operand stack.** `vm/src/runtime/value_stack.rs` `scan_object_refs_conservative` (~2020)
   applies the same three-candidate probe to every slot, `KIND_LONG` / `KIND_DOUBLE` included.
   Same fix (honour `kinds`, root minted handles only), same gate.
3. **`--compatible` roots primitive longs on the ordinary operand-stack scan.**
   `value_stack.rs` ~1936-2007 with `vm/src/memory/roots.rs` ~1848: `minted_only_primitives =
   is_jdk_only()`, so in `--compatible` a `KIND_LONG` slot whose bits pass the loose `is_heap_addr`
   test is rooted on every cycle (interior values reach `forward_object` and are refused). The
   rewrite side is minted-only, so values are not corrupted; only retention. AGENTS.md: both modes
   target HotSpot, so the minted-only rule should be the default in both (keep
   `CRATONVM_LONGREWRITE_LOOSE` as the escape hatch). Rows: the probe of item 1 under
   `--compatible`.
4. **Narrow soundness risk: the locals remap rewrites an unproven `OTHER` slot.**
   `Frame::update_local_refs` (~4220-4255): an `LKIND_OTHER` slot with no object tag is rewritten
   when any of its three candidates is a `pointer_map` key and the new address is an object; no
   type proof. `lkind_of_compact` marks only `CompactTag::Long`; an untagged `long` that reads as
   `CompactTag::Double` (the `istore`/`lstore` compact fast arms, the OSR restore) leaves the slot
   `OTHER`, so a primitive whose bits equal a moved object's old address is rewritten. Fix: gate
   the lost-tag rewrite on a type proof (the typed-locals mask or the verifier row; see the
   precise-interpreter-frames proposal).
5. **Doc.** `roots.rs` (~126) says conservative locals are "sound ONLY where the collection
   provably does not relocate", but with moving young on the promise holds only while it is off
   (`gc_quiescence.rs` ~2269). Under moving young a conservatively found value pins at best; say so.
