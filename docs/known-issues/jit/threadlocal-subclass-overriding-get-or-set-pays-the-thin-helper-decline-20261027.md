> **j27 wave 8 (lane W8CHA, 2026-10-28): REVIEWED, PENDING HOST VERIFICATION (own-site doors).** No code change. All three bind sites ask one decision (`thread_local_thin_bind::receiver_counts_refuse_thin_bind`): `ir_tier_attempt`'s `get` / `set` binds and `build_single_pass_tables`' callee-direct binds pass the compiling method's profile at the site pc; the spliced rows are unbound in `unbind_refused_spliced_thin_calls`. Positive evidence only (no profile, no dominant class or no VM answer binds as before), so a wrong refusal costs speed, never an answer. `InheritableThreadLocal` sites take the same rule (its own kind is admitted, an overriding subclass is not). Residue unchanged: `THREAD_LOCAL_GET_DIRECT_SITES` counts a spliced row the pass later unbinds (diagnostic only). Host check still owed for the own-site doors: `J27TlOverrideBind` facts = HotSpot (wave-1 block) under default and `CRATONVM_JIT_TL_THIN_OVERRIDE_DECLINE=0`, `CRATONVM_DBG_JITC=1` shows `tl-thin-bind door=ir ... REFUSED` and `door=single-pass` where that door compiles.

> **j27 wave 2 (lane TCARRAY, 2026-10-28): FIXED IN CODE (the spliced half), HOST-VERIFIED s3 (2026-10-28).** Host facts (checkpoint commit `543e6f12ba`, host-verified on snapshot `bb68e37943`): `J27TlOverrideSplice` SAME as HotSpot, and the decline engages. Not shown there: the `-Dj27.time=true` default-vs-`=0` timing. The spliced-body thin row now takes the same refusal. Rather than give the name-only `spliced_thin_helper` closure a pc, the decision runs where the spliced rows are merged (`ir_tier_attempt`, just before `ir_direct_calls.extend(sub_direct_calls)`): new `thread_local_thin_bind::unbind_refused_spliced_thin_calls` walks the site's bound rows, and a row bound to `thread_local_get` / `_set` whose callee receiver counts (`IrInlineTables::spliced_receiver_seeds`, the callee profile `append_ir_inline_site` already re-keys at the combined pc for `ThreadLocal` rows) name a dominant class the VM declines becomes a virtual call (`sub_vcalls`, its arity from `invoke_info`), so it gets the inline cache seeded from the same counts. The refusal core is shared with the own-site doors (`receiver_counts_refuse_thin_bind`, split out of `thread_local_thin_bind_refused`), so the same switch `CRATONVM_JIT_TL_THIN_OVERRIDE_DECLINE` (default ON) kills both; door name `splice` in `CRATONVM_DBG_JITC`. Unit test `jit/src/thread_local_thin_bind.rs` `j27_tl_thin_override_tests::j27_a_spliced_thin_call_on_an_overriding_class_becomes_a_virtual_call` (crate `cratonvm-jit`). Known residue: `THREAD_LOCAL_GET_DIRECT_SITES` still counts a spliced row the pass later unbinds. Probe `tools/probes/jit/j27/J27TlOverrideSplice.java` (`--jdk-only`), expected HotSpot 25: `fact over-sum 1400000`, `fact plain-sum 600000`, `fact gets 400000`, `fact sets 400000`, `fact last 3`; `CRATONVM_DBG_JITC=1` prints `tl-thin-bind door=splice ... REFUSED`; `-Dj27.time=true` `spliced-override` default vs `=0` (the default must not be slower; facts identical).

> **j27 wave 1 (lane TYPECHK, 2026-10-28): PARTLY, PENDING HOST VERIFICATION.** New `jit/src/thread_local_thin_bind.rs` (`thread_local_thin_bind_refused`): a `ThreadLocal.get()` / `set(Object)` site whose receiver profile names a dominant class (>= 50 %, `profile::dominant_receiver`) that the VM's one-class query (`DirectHelperTable::thread_local_receiver_kind`, the helper's own verdict) answers as declined -- `get`: the bytecode kind; `set`: not set-admitted -- is no longer bound to the thin helper and stays an ordinary virtual call (inline cache / guarded splice). Wired at `ir_tier_attempt`'s two binds (door `ir`) and the single-pass callee-direct binds in `build_single_pass_tables` (door `single-pass`). NOT done: the spliced-body thin row (`spliced_thread_local_thin_cell` closure in `intern_inline_invoke_targets`), which has no callee profile pc at hand. Switch `CRATONVM_JIT_TL_THIN_OVERRIDE_DECLINE` (default ON, `=0` binds as before). Positive evidence only: no profile / no dominant class / no VM answer binds as before. Probe `tools/probes/jit/j27/J27TlOverrideBind.java` (`--jdk-only`), expected HotSpot 25: `fact override-gets 1600000`, `fact override-sets 1600000`, `fact g-value 48`, `fact s-value 9`, `fact plain-value 9`; with `-Dj27.time=true` the `tl-get-override` / `tl-set-override` rows should drop from ~15-28 us to within a few x of HotSpot (92 / 277 ns). Engagement: `CRATONVM_DBG_JITC=1` prints `tl-thin-bind door=ir ... REFUSED (the class overrides)`. Facts identical under `=0`.

Status: FIXED IN CODE, both halves (own-site doors: j27 wave 1, lane TYPECHK, PENDING HOST VERIFICATION;
spliced half: j27 wave 2, lane TCARRAY, HOST-VERIFIED s3). Filed OPEN (found 2026-10-27 while retiring
`docs/internal/fixed-bugs/jr21-tl3-threadlocal-subclass-get-takes-the-thin-helper-FIXED-20261014.md`)

# A `ThreadLocal` subclass that overrides `get()` / `set()` pays the thin helper's by-name decline on every call

Area: `jit/src/lib.rs` (`ir_tier_attempt`'s and the callee-direct path's `ThreadLocal.get` / `set` thin
binds, `direct_native_helper(direct_helpers.thread_local_get / _set, ...)`), `vm/src/jit/helpers/fast_native.rs`
(`jit_thread_local_get_direct_body` / `jit_thread_local_set_direct_body`: the decline edge's
`jit_invoke_dispatch(&THREAD_LOCAL_*_DIRECT_INFO)`)
Found by: `R21TlsubRows` rows `tl-get-override` / `tl-set-override` (host 64.177.43.150, `--jdk-only`, ZGC)
Severity: LOW (correct; a subclass overriding `get` / `set` is rare: `InheritableThreadLocal` has its own
kind, `withInitial` and the `initialValue` idiom override neither)

## The numbers (ns per 16-op kernel call, one run)

| row | CratonVM | HotSpot 25 |
|---|---|---|
| `tl-get-override` (covariant `int[] get()`, reached through its `Object get()` bridge) | 15 682 | 92 |
| `tl-set-override` (covariant `set(int[])`, reached through its `set(Object)` bridge) | 28 265 | 277 |

## Why

Every `invokevirtual ThreadLocal.get()` / `set(Object)` site under `--jdk-only` binds the thin helper,
whatever its receiver, because `ThreadLocal` is not final and the helper screens the receiver class itself.
For a class that overrides the method (a bridge counts, since the 2026-10-27 fix) the helper declines on
EVERY call to `jit_invoke_dispatch` with a shared synthetic `JitInvokeInfo`: a by-name virtual resolution
per call, with no site inline cache, ~1 us. The override itself then runs and calls `super.get()` /
`super.set()`, whose own site is the expansion or the helper again.

Until the 2026-10-27 fix the helper and the compiler admitted such a class by its non-bridge methods
only, so `tl-set-override` stored past the override: the override ran 10 times where HotSpot runs it
5 360 010 (a wrong answer; fixed, `tl_set_receiver_ok` and the bridge-inclusive walks).

## What a fix looks like

Do not bind the thin helper at a site whose receiver profile names a dominant class the VM's one-class
query (`DirectHelperTable::thread_local_receiver_kind`) answers as its own bytecode (`get`) or as declaring
`set` (`set`): leave it the ordinary virtual call, so the inline cache (or a guarded splice of the
override, the receiver being a profiled exact class) serves it. Three bind sites ask the same question:
`ir_tier_attempt`'s two (top-level `get` / `set`), the spliced-body thin row
(`intern_inline_invoke_targets`), and the callee-direct path. Probe: the two rows above, target within a
few x of HotSpot; facts (`override-gets`, `override-sets`) identical to HotSpot in every arm.
