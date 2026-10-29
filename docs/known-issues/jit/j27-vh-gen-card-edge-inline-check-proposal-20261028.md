> **j27 wave 8 (lane W8FFM, 2026-10-28): REVIEWED, PENDING HOST VERIFICATION.** No code change. `jit/src/ir_lower/vh_gen_card_edge.rs`: the PRE check and the inline card store write R10 / R11 and the flags only, so the no-call PRE path reaches the access with RAX = holder and RDX = value intact, and the access reloads `expected` into R10; the call path reloads RAX / RDX after the call; POST re-forms the card from the holder's home and the view's current words; the old -> old edge reloads `expected` and rejoins the screen's own access, which `the_screens_access_is_the_tail_of_the_core_bytes` pins. `jit/src/x64/vh_gen_card_sp.rs` admits only call-surviving operand slots and keeps the result on the machine stack across POST (the G1 edge's shape). One shared property, not new here: a take-over freeze between PRE's card-address arithmetic and its card-byte compare uses a card address formed from the pre-freeze view (as the gated `putfield` arm does); POST then re-forms it from the current view, so the edge stays covered. Verify: this page's soak on Linux at `-Dj27.rounds=2000`, as below.
>
> **j27 wave 4 (lane VHCARD, 2026-10-28): FIXED IN CODE, PENDING HOST VERIFICATION.** Measured locally on Windows (builder lane); the numbers below are from `cvm-vhcard-1.exe`.
> - **IR tier** (`jit/src/ir_lower/vh_gen_card_edge.rs`, switch `CRATONVM_JIT_VH_CONST_GEN_CARD_INLINE`, perf, default ON, `=0` restores VHCOMP's two-call edge). The card edge now runs the gated `putfield` arm's inline check:
>   - the range filter: a holder outside the view takes the call; old -> old rejoins the screen's own access;
>   - PRE: the dirty test, then the inline card store (`emit_ir_card_pre_check` with `CRATONVM_JIT_CARD_STORE_BARRIER`);
>   - the access, with `expected` reloaded into R10 after PRE;
>   - POST: the holder reloaded from its home, the range test against the view's current words, the dirty test.
>   - `jit_write_barrier` is called only on the PRE slow edges (clean card with no card-store form, scan-bound refusal, outside the view) and on POST's clean / outside edges.
>   - It is always the classic PRE + POST shape, never the post-only one. The post-only take-over completion needs the holder in a register from the store to the card byte, which the access does not keep (RAX = its result). The module comment carries the take-over argument.
>   - The result is also re-moved into its register after each barrier call. VHCOMP's edge reloaded only RAX.
> - **Single-pass twin** (`jit/src/x64/vh_gen_card_sp.rs`, wired in `Compiler::emit_sp_vh_const_prefix`; switch `CRATONVM_JIT_SP_VH_CONST_GEN_CARD_EDGE`, perf, default ON, `=0` restores the decline; also off under `CRATONVM_JIT_VH_CONST_GEN_CARD_EDGE=0`).
>   - The same edge, built from the backend's own `emit_gen_card_barrier` PRE / POST pair. The result is kept on the machine stack across POST, as the G1 post edge does.
>   - Admitted only when the receiver and the values live in frame slots or callee-saved registers. The barrier's slow edge reloads them after its call.
> - **Measured** (P-cores, `VhcaeG1Rows -Xmx2g -XX:+UseGenerationalGC`, ns/op): see the lane report's table. In short, `vh-cas-fresh-val` went from 34-50 to about 17 and `vh-setvol-fresh-val` from 30-41 to about 18. HotSpot is 13 / 13.
> - **Soundness:** `J27VhGenCardStress -Xmx256m -Xmn4m -Dj27.rounds=400` passed 40/40 runs in each of these configurations:
>   - `--jdk-only`: plain, `CRATONVM_DBG_GC_STRESS=1`, `CRATONVM_DBG_XT_FORCE_TAKEOVER=1`, and take-over + stress;
>   - `--compatible`: plain, and take-over + stress.
> - **The soak catches a lost card.** A scratch build that skipped both card marks (never in the tree) failed 20/20 runs: wrong ids, plus heap-guard errors in the young sweep.
> - **Facts:** `J27VhStaticCompiled`, `J27VhGenCardStress`, `VhcaeG1Rows` and `VhcaeG1Stress` match HotSpot 25 in 24/24 cells (Gen / G1 / ZGC x `--jdk-only` / `--compatible`).
> - **Unit tests** (crate `cratonvm-jit`):
>   - `ir_lower::vh_gen_card_edge::j27_vhcard_inline_edge_tests`: the switch; the screen's access is the core bytes' tail; the inline edge has the PRE and POST card compares and two slow calls, and the kill / no-view has none.
>   - `x64::vh_gen_card_sp::j27_vhcard_sp_edge_tests`.
> - **Verify on the host:**
>   - `CRATONVM_DBG_JITC=1` prints `generational card edge inline=true` (IR) and `vh-const-inline (sp) generational card edge`.
>   - Repeat the soak on Linux, longer (`-Dj27.rounds=2000`).
>   - A/B the rows against `CRATONVM_JIT_VH_CONST_GEN_CARD_INLINE=0`.
>
> Expected HotSpot 25 (`-Dj27.rounds=400`): `fact vh-gen-stress-ok true`, `fact vh-gen-stress-sum 1342175641600`.

# Proposal: inline PRE/POST card check on the generational constant-`VarHandle` card edge

Status: FIXED IN CODE (j27 wave 4, lane VHCARD; measured locally on Windows), PENDING HOST VERIFICATION.
Proposed j27 wave 2, lane VHCOMP, 2026-10-28.
Area: `jit/src/ir_lower/vh_gen_card_edge.rs`, `jit/src/ir_lower.rs` `emit_ir_putfield_card_arm` / `emit_ir_card_pre_check` / `emit_ir_card_post_compare`.

## Evidence

`VhcaeG1Rows -XX:+UseGenerationalGC` on Windows after `CRATONVM_JIT_VH_CONST_GEN_CARD_EDGE`:
`vh-cas-fresh-val` 34-50 ns, `vh-setvol-fresh-val` 30-41 ns; the same-shape `putfield-fresh-val`
4-6 ns; HotSpot 13 ns. The edge pays two `jit_write_barrier` calls per write.

## What would close it

Reuse the gated `putfield` arm's inline card CHECK for the edge: card address of the holder from
the view, PRE (`emit_ir_card_pre_check`, the inline card store under `CRATONVM_JIT_CARD_STORE_BARRIER`),
the access bytes, POST (`emit_ir_card_post_compare` + `emit_ir_card_post_clean`), keeping the
calls only for the edges the arm sends to its helper. The register contract differs from the
`putfield` arm (a CAS needs R10 = `expected`, which the card arithmetic uses): reload `expected`
from its home after the PRE check. Soak with `J27VhGenCardStress` as the page says.
