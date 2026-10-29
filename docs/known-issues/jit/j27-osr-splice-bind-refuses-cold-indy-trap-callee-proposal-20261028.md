# j27 proposal: the OSR splice binder refuses a callee whose only indy trap sits on a cold throw arm

> **j27 wave 8 (lane W8OSR, 2026-10-28): REVIEWED, NOT FIXED (premise void, as INDYTRAP found), PENDING HOST VERIFICATION.** No code change. Re-checked the two conclusions below. OSRSUPER: the binder's `has_indy_trap` refusal must stay, because a raw `CALL` has no helper that could resume the reason-8 stash (`deopt_stubs.rs` item (d)). INDYTRAP: javac's concat never plants that trap; the bridge is built from the pool alone. So a host line showing `Base.pos` still unbound points at one of `osr_splice_direct_bind`'s other gates, not at the indy. The host recipe in the INDYTRAP block is the check: no `indy trap pc=` line naming `J27IndyTrapRows` / `J27OsrSuperRows`.

> **j27 wave 4 (lane SPLICECAT, 2026-10-28): NOT FIXED here (premise void, see INDYTRAP below); the splice half filed by INDYTRAP is FIXED IN CODE behind the opt-in `CRATONVM_JIT_SPLICE_CONCAT_BRIDGE=1`, PENDING HOST VERIFICATION.** The resolver now admits an argument-taking concat bridge site as an `ir_indy_rows` row in both tiers, so a callee holding `"neg: " + v` is no longer refused with `invokedynamic (capturing or non-reference site: no constant word)`; details, probe `tools/probes/jit/j27/J27SpliceConcatRows.java` and its expected HotSpot output are in [j27-splice-refuses-callee-with-arg-taking-concat-proposal-20261028](j27-splice-refuses-callee-with-arg-taking-concat-proposal-20261028.md). This page's own question (the OSR direct bind of `Base.pos`) is untouched: `osr_splice_direct_bind` was not edited. Host check for this page: with the switch on, `CRATONVM_DBG_JITC=1` on `J27OsrSuperRows` may show a `splice-concat-row ADMITTED ...Base.pos(I)...` line only from a single-pass resolution (the optimizing tier masks that cold throw arm, so it has no indy left; the single-pass splice may still refuse the arm's `new`/`athrow`), and `J27IndyTrapRows` / `J27OsrSuperRows` stdout must stay identical to HotSpot.

> **j27 wave 4 (lane INDYTRAP, 2026-10-28): NOT FIXED -- the premise does not hold for javac's concat; no trap to remove, PENDING HOST VERIFICATION.** By reading, a javac `makeConcatWithConstants` site gets NO reason-8 trap whether or not it was ever linked: every single-pass door builds its `indy_info` row through `interned_jit_indy_bridge_site` -> `make_jit_indy_bridge_site_from_parts` -> `make_jit_string_concat_site_from_parts` (`vm/src/runtime/invokedynamic.rs`), which builds the concat bridge site from the constant pool alone (recipe, `\2` constants, call type; no linkage, no Java run, so no class-init or bootstrap side effect can move), gated by no switch (the `CRATONVM_JIT_INDY_BRIDGE` kill switch gates only the generic, non-concat half); the `0xba` arm then emits the bridge call (`indy_site_is_bridged`) and `indy_rows_have_trap` / `has_indy_trap` stay false. `"neg: " + v` (`(I)String`, recipe `neg: \1`) is such a site, so `Base.pos` is not refused by the `has_indy_trap` gate; the "compile-time recipe bridge" this page asks for is what the code already does. Only shapes javac never emits keep the trap (`jit_concat_site_is_pool_decided`: a `Class` / `MethodType` / `MethodHandle` / dynamic constant, tags that disagree with the call type, more than 200 slots, an odd return type) and they must keep it for HotSpot parity (`interface X`, `--jdk-only` factory errors). Hence no switch: an opt-in switch over code that already runs unconditionally would be dead. **What this lane did:** (1) unit tests pinning the premise, `vm` crate `runtime::invokedynamic::j27_indytrap_cold_concat_tests::{an_unlinked_javac_concat_is_bridged_and_renders_the_factory_text, a_shape_javac_never_emits_keeps_its_trap}` (an unlinked javac pool through the weakest, CP-only door renders `null`, `char`, `float`, `double`, UTF16, `\2` constants and a folded constant as HotSpot does); (2) a `CRATONVM_DBG_JITC` line for the trap the single-pass `0xba` arm plants (`jit/src/x64/op_invoke.rs`, twin of the existing `indy bridge pc=` line): `[cratonvm-jitc] indy trap pc=<bci> args=<n> ret=<tag> site=<hex> in <method>`; (3) probe `tools/probes/jit/j27/J27IndyTrapRows.java` (expected output below). **The real reason a `"neg: " + v` callee is not SPLICED** is elsewhere and filed as [j27-splice-refuses-callee-with-arg-taking-concat-proposal-20261028](j27-splice-refuses-callee-with-arg-taking-concat-proposal-20261028.md): `resolve_inline_site_from` admits only `()L` constant-word indy sites in a spliced body, so any arg-taking concat refuses the splice (`invokedynamic (capturing or non-reference site: no constant word)`), and the IR table check (`ir-splice-indy-row-mismatch`, `arg_entries != 0`) would refuse it too. If the host shows the direct bind of `Base.pos` still refused, the cause is one of `osr_splice_direct_bind`'s other gates (`try_jit_compile_callee_from_site_in_place` declining, `direct_call_requires_dispatch_of`, `osr_callee_bars_direct_call`), not the indy trap. **Host recipe:** `CRATONVM_DBG_JITC=1 cratonvm --cp <dir> J27IndyTrapRows 2>&1 | grep -E 'indy (bridge|trap) pc='` must show `indy bridge pc=` lines and NO `indy trap pc=` line naming `J27IndyTrapRows` (any such line is a counter-example to this analysis: file it with its `site=`), in both modes; the same grep on `J27OsrSuperRows` for `Base.pos`; and `J27IndyTrapRows` stdout must diff clean against HotSpot in `--jdk-only`, `--compatible` and `--nojit`.
>
> Expected HotSpot 25 output of `J27IndyTrapRows` (derived by hand from the JLS / `Float.toString` / `Double.toString` rules; confirm with the reference run). `\uXXXX` below is the probe's own escape, printed literally:
>
> ```text
> fact indytrap-warm 102387360
> fact indytrap-neg java.lang.IllegalArgumentException neg: -5
> fact indytrap-null java.lang.IllegalStateException s=null;o=null
> fact indytrap-char java.lang.IllegalArgumentException c=A
> fact indytrap-float java.lang.IllegalArgumentException f=0.33333334,1.0E10
> fact indytrap-double java.lang.IllegalArgumentException d=0.30000000000000004,-0.0,1.0E-5
> fact indytrap-utf16 java.lang.IllegalArgumentException Ω=-1 Ω
> fact indytrap-const java.lang.IllegalArgumentException <\u0001>-9223372036854775808!K
> fact indytrap-order-before []
> fact indytrap-order java.lang.IllegalArgumentException tag=42 o=T
> fact indytrap-order-after [clinit-Lazy, toString]
> fact indytrap-splice-sum 3500000
> fact indytrap-splice-throw java.lang.IllegalArgumentException neg: -3
> fact indytrap-splice-frames J27IndyTrapRows$Base.pos<J27IndyTrapRows$Mid.pos<J27IndyTrapRows.spliceLoop
> ```

> **j27 wave 4 (lane OSRSUPER, 2026-10-28): NOT FIXED, kept as the design.** Checked by reading whether `osr_splice_direct_bind` could simply accept a callee whose only indy trap is on a never-run cold throw arm. It cannot, soundly: the `has_indy_trap` refusal is one of the codebase-wide publication gates of the 2026-07-07 resume fix (`jit/src/x64/deopt_stubs.rs`, item (d): "machine code never direct-calls an indy-trap artifact (no helper there could resolve it)"). The reason-8 trap stashes a frame that only a dispatch helper's `try_resume_trapped_callee` resumes; a raw `CALL` has no such helper, and a cold arm is exactly where the trap eventually fires (the probe's `osr-user-throw` row throws it). Where the trap sits does not change that, so no cold-arm exemption is sound. The fix has to remove the TRAP, not the gate: bridge the unlinked concat site at compile time from its recipe (`invokedynamic::make_jit_string_concat_site_from_recipe` already builds a concat bridge site from recipe + constants without the call site being linked; the single-pass 0xba arm would then emit the bridge call and `has_indy_trap` stays false), or re-link and recompile the callee at bind time. Either is a change to the indy lowering (`compile_osr_body` / the method-entry doors' indy resolvers), not to the binder, and needs a build to show the bridged concat matches HotSpot's message byte for byte. The VM half of P2 is wired (`compile_osr_body`), so with `CRATONVM_JIT_OSR_GUARDED_SUPER_BIND=1` the host DBG line shows this case as `osr-splice-guarded-super-bind` absent for `Mid.pos` (the bound resolver returns no body: the bind of `Base.pos` refused) while `HMid.pos` is ADMITTED.

| | |
|---|---|
| **Status** | NOT FIXED, premise void for javac's concat (j27 wave 4, lane INDYTRAP), PENDING HOST VERIFICATION; the splice half it led to is FIXED IN CODE behind the opt-in `CRATONVM_JIT_SPLICE_CONCAT_BRIDGE=1` on `j27-splice-refuses-callee-with-arg-taking-concat-proposal-20261028.md` (lane SPLICECAT). Proposal filed 2026-10-28 by j27 wave 4 lane OSRSUPER; nothing built on this page. |
| **Area** | JIT: `osr_splice_direct_bind` (`vm/src/runtime/interpreter/jit_bridge.rs`), single-pass indy trap (`CompiledMethod::has_indy_trap`). |
| **Parent** | [j27-small-call-frame-arms-and-osr-super-bind-proposal-20261028](j27-small-call-frame-arms-and-osr-super-bind-proposal-20261028.md) (P2, item 3) |
| **Probe** | `tools/probes/jit/j27/J27OsrSuperRows.java` row `osr-user-throw` (`Base.pos`) against `osr-helper-throw` (`HBase.pos`) |

## Evidence (by reading, not run)

`osr_splice_direct_bind` returns `None` for any compiled callee with `pin.has_indy_trap`. A user
super method such as

```java
Base pos(int v) { if (v < 0) throw new IllegalArgumentException("neg: " + v); p = v; return this; }
```

whose cold arm has never run is compiled with the `invokedynamic` concat still unlinked, so the
single-pass body carries an unconditional indy trap at that pc. The bind is refused, and with it
every OSR-door splice whose body calls `super.pos(v)`: the statically bound pass and, under
`CRATONVM_JIT_OSR_GUARDED_SUPER_BIND=1`, the guarded pass. The trap is reached only on the
throwing path, so the body is correct to CALL: the trap deopts into the interpreter, which links
the indy and throws. The JDK's own setters are not affected (java.base builds messages with
`StringBuilder`, in a separate `createPositionException`).

## What would fix it

Let the binder accept a body whose indy traps all sit on cold throw arms (`ir::cold_throw_arms`
over the callee's bytecode names the arms; an arm that ends in `athrow` with the indy inside it),
or re-link the concat site at bind time (`prime_jit_indy_constant_word` / the linked-site bridge)
and recompile the callee without the trap. Check first why the bind refuses `has_indy_trap` at
all (`git log -S'has_indy_trap' -- vm/src/runtime/interpreter/jit_bridge.rs`): the
`jit-invokedynamic-groovy-regression` notes say a baked raw CALL into a trap body lost frames,
which a fix must keep impossible. Measure on `J27OsrSuperRows` `-Dj27.timing=true` before and after.
