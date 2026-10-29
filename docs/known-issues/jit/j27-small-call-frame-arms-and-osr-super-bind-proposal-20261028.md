# j27 proposal: measure the opt-in frame arms on the small-JDK-call workload; let the single-pass OSR guarded pass bind `super.m()`

> **j27 wave 8 (lane W8CHA, 2026-10-28): REVIEWED (P2), PENDING HOST VERIFICATION. P1 decided (no flip), P3 not proposed: nothing to build.** No code change. Re-read `jit/src/osr_guarded_super_bind.rs`, `plan_osr_door_guarded_splices_bound` (`jit/src/lib.rs`) and the `compile_osr_body` wiring (`jit_bridge.rs`): the binder-carrying resolver is asked only after the plain one refused and only with the switch on (`super_bound` filtered by `osr_guarded_super_bind_enabled`; `None` under `CRATONVM_JIT_OSR_SPLICE_CALLS=0`); `body_calls_only_bound_super` refuses any unbound call, any static exit row, a constructor, a call on the body's own class and a kind-1 pin that is not opcode `0xb7`, recursively through nested and bimorphic bodies. Note for the host run: a refused re-resolve may still have asked the binder for the super method's entry (a pending compile request); harmless, but it can show as an extra `Buffer.position` compile in `CRATONVM_DBG=jit-compiled` with the switch on. Host recipe unchanged (OSRSUPER block below).

> **j27 wave 4 (lane FRAMEAB, 2026-10-28): P1 MEASURED (Windows, local), NO FLIP.** The two SIZED switches engage on this workload (`Buffer.position(I)` frame 544 -> 352 bytes, fresh clear 176 -> 128) but move no row beyond the run-to-run spread: paired sized/base ratios 0.92-1.12, wins 10-16 of 24 interleaved pairs (a coin flip). `CRATONVM_JIT_IR_DEOPT_IMAGE_SIZED` and `CRATONVM_JIT_IR_SAVE_BANDS_SIZED` stay default OFF. The other four were already default ON (j27 flip); turning them off (`=0`) is mildly slower (base wins 5-8 of 9 pairs on most rows), so the flip holds. Facts identical to the base arm in every arm, both modes, and the GC battery is clean. Numbers and recipe: section "P1 results" below. P2 (lane OSRSUPER) and P3 unchanged.

> **j27 wave 4 (lane OSRSUPER, 2026-10-28): P2 FIXED IN CODE behind the opt-in (planner half and VM wiring), PENDING HOST VERIFICATION. P1, P3 untouched.**
> **Why the "never downgraded" rule exists** (read-only `git log -S` / `show`): it was written by `facd6c8ff8` (2026-08-18, "direct-bind a spliced call") at the method-entry doors, where the binder closure "is declared further down this function, and a spliced body reached through a receiver guard is planned before it exists. The consequence is a REFUSAL, never a downgrade". A *downgrade* is a spliced call emitted through the blind `jit_invoke_dispatch` (measured 47 -> 163-266 ns/iter, 3.5x). So the rule forbids an UNBOUND call in a splice, not a bound one; the `None` is a declaration-order accident. Lane mega copied it to the single-pass OSR door (`24b8dc53b2`, r16 w13), where the door's binder `osr_splice_bind` already exists before the guarded pass.
> **What changed (jit crate only).** New `jit/src/osr_guarded_super_bind.rs`; `plan_osr_door_guarded_splices` is now a wrapper over the new `plan_osr_door_guarded_splices_bound(.., super_bound_resolve: Option<ReceiverResolve>)` (`jit/src/lib.rs`). Under `CRATONVM_JIT_OSR_GUARDED_SUPER_BIND=1` (opt-in: the first single-pass guarded variant with a call in it, unbuilt and unmeasured) a receiver body the plain resolver refused is asked of the binder-carrying resolver and kept only when every call left in it, nested bodies included, is a **bound** `invokespecial` (opcode `0xb7`, kind 1, not `<init>`, named on a class other than the body's own: a superclass), with no static exit rows (`body_calls_only_bound_super`). A body the plain resolver admits is never re-resolved. Debug line: `[cratonvm-jitc] osr-splice-guarded-super-bind ADMITTED|REFUSED <site> receiver=<id> body=<..> calls=[pc:kind:target[bound|unbound]]`. The `osr-splice-guarded-refused` hint names the switch.
> **Soundness argument** (module doc): selection is static (JVMS 6.5; the VM resolver's `classify_invoke_site` already refuses or substitutes the owner before binding); the call goes through the same splice walk as a call inside a statically bound OSR splice (`push_inline_scope` per variant, `JitInvokeInfo` service copy); the bound entry reaches the OSR artifact's `_direct_callee_entries` (`intern_osr_door_splice_targets` interns the variants), so withdrawing the super method's body evicts the OSR body; the receiver body keeps its `SpeculatedReceiver` dependency; a new subclass misses the exact class-id guard into the unchanged dispatch.
> **VM wiring (applied, same wave, orchestrator's go-ahead).** `vm/src/runtime/interpreter/jit_bridge.rs` `compile_osr_body`, guarded block only: new `receiver_super_bound_resolver` (`resolve_receiver_inline_site(.., Some(&osr_splice_bind as InlineDirectBind<'_>))`), passed as `super_bound` (`None` under `CRATONVM_JIT_OSR_SPLICE_CALLS=0`) to `cratonvm_jit::plan_osr_door_guarded_splices_bound`; the "(no direct bind inside a guarded body ..)" comment now states the rule and points at `cratonvm_jit::osr_guarded_super_bind`.
> **Item 3 (indy concat on the cold path).** Not a blocker for the NIO shape: `Buffer.position(I)` is CALLED (bound), not spliced, and its message is built in the separate `createPositionException` (java.base is compiled with `-XDstringConcat=inline`, so it is a `StringBuilder` chain there, not an indy; confirm with `javap -c -p --module java.base java.nio.Buffer`). The cold-throw-arm machinery (`ir::mask_cold_throw_arms`, `cold_throw_arms`) is optimizing-tier only and does not apply to this single-pass door. One real gap for USER code: a super method with an inline `invokedynamic` concat on its cold arm that has never run is compiled with an indy trap, and `osr_splice_direct_bind` refuses any `has_indy_trap` body, so that guarded body stays refused (probe row `osr-user-throw`, `Base.pos`). Not fixed: exempting a cold-arm trap from the binder's gate is unsound (a raw CALL cannot resume a reason-8 trap frame); the fix is to bridge the concat instead, see [j27-osr-splice-bind-refuses-cold-indy-trap-callee-proposal-20261028](j27-osr-splice-bind-refuses-cold-indy-trap-callee-proposal-20261028.md).
> **Probe** `tools/probes/jit/j27/J27OsrSuperRows.java` (expected HotSpot 25 lines in its header): NIO setter loop, a cold-arm IAE thrown inside the OSR'd loop (message, frames to `main`, buffer state), a mid-loop `Late` subclass overriding `Mid.pos`, user super with inline indy concat and with a helper-built message.
> **Host recipe.** (a) Facts: `java` vs `cratonvm` both modes, `--nojit`, and each with `CRATONVM_JIT_OSR_GUARDED_SUPER_BIND=1`; all diffs empty. (b) Engagement: `CRATONVM_JIT_OSR_GUARDED_SUPER_BIND=1 CRATONVM_DBG_JITC=1 CRATONVM_DBG=jit-compiled cratonvm --cp <dir> J27OsrSuperRows 2>jitc.txt; grep -E 'osr-splice-guarded-(super-bind|planned|refused)' jitc.txt` -- expect `super-bind ADMITTED java/nio/ByteBuffer.position(I)Ljava/nio/ByteBuffer; .. calls=[2:1:java/nio/Buffer.position(I)Ljava/nio/Buffer;[bound]]` followed by `osr-splice-guarded-planned java/nio/ByteBuffer.position.. how=guarded`, the same for `J27OsrSuperRows$HMid.pos`, and without the switch the old `osr-splice-guarded-refused .. CalleeUnresolved` lines. (c) Unit tests: `cargo test -p cratonvm-jit -j 5 osr_guarded_super_bind` and `osr_guarded_refusal_hint`.

| | |
|---|---|
| **Status** | PARTLY: P2 FIXED IN CODE behind the opt-in `CRATONVM_JIT_OSR_GUARDED_SUPER_BIND=1` (j27 wave 4, lane OSRSUPER), PENDING HOST VERIFICATION; P1 measured locally by lane FRAMEAB (no win, SIZED switches stay OFF); P3 not built. Proposal filed 2026-10-28 by j27 wave 3 lane SMALLCALL. |
| **Area** | JIT: optimizing-tier frame layout and clear (`jit/src/ir_lower.rs`), single-pass OSR door guarded splices (`jit/src/lib.rs` `plan_osr_door_guarded_splices`, `vm/src/runtime/interpreter/jit_bridge.rs` `compile_osr_body`). |
| **Parent** | [small-jdk-method-call-overhead-...-20261025](small-jdk-method-call-overhead-charsetdecoder-decode-25-80x-tomcat-methodperformance-and-http2-8-2-wall-20261025.md) |
| **Probe** | `tools/probes/jit/j27/J27SmallCallRows.java` (`-Dj27.timing=true`) |

## P1. The frame arms, on the workload they were waiting for

Every optimizing-tier body reserves, whatever its size: 48 bytes reserve and shadow, the 256-byte deopt
register image (`ir_deopt_regs_bytes`) unless no exit can name a register
(`CRATONVM_JIT_IR_DEOPT_IMAGE_EXITLESS`, ON), one save slot per register of the callee-saved XMM and GPR
files, 40 bytes bookkeeping, one word per Java local. A three-instruction covariant bridge
(`ByteBuffer.limit(I)Buffer`) therefore has a 0x230-byte frame and clears about 384 bytes of it per call
(twelve 32-byte stores, parent page, third pass). Opt-in arms that shrink it already exist, each measured
neutral on kernels with the note "re-measure only on a frame-bound workload":

| Switch | What it removes |
|---|---|
| `CRATONVM_JIT_IR_DEOPT_IMAGE_SIZED` | the image past the highest deopt-nameable GPR (all of it for a body with no callee-saved residency) |
| `CRATONVM_JIT_IR_SAVE_BANDS_SIZED` | unused XMM / GPR save slots |
| `CRATONVM_JIT_IR_DEOPT_IMAGE_CLAIM` | the image's GPR half from the clear (128 bytes) |
| `CRATONVM_JIT_IR_FRAME_PARAM_LOCALS_ONLY` | non-parameter Java-local words |
| `CRATONVM_JIT_IR_FRESH_CLEAR_COVER` | the clear as a minimum straight-line cover |
| `CRATONVM_JIT_IR_LEAF_FRAME_ELIDE` | the whole clear and bookkeeping of a leaf frame (`Buffer.position` itself has a cold call, so likely not a leaf) |

The setter chain is the frame-bound workload. Measure per the parent page's host recipe (c): interleaved,
P-cores, `time` lines and `instructions:u` per op by doubling N, three arms (base; the two SIZED; all six).
Decide per switch: one that moves a row by more than the run-to-run spread with facts unchanged and the GC
battery clean (ZGC / G1 `-Xmx256m` / generational, 4 loaded copies x 20) is flipped default ON with its
`=0` kill; one that does not stays OFF (never deleted).

### P1 results (j27 wave 4, lane FRAMEAB, 2026-10-28)

**Setup.** Windows 11, local build of this worktree (`cargo build --release -j 5 -p cratonvm-cli`, LTO off,
CGU 16), the shell and its children pinned to the P-cores (affinity `0xFFFF`), `-Xmx256m`, default
`--jdk-only`. Other lanes were building on the same machine (2-4 `rustc` processes), so wall times are
noisy; no `perf` here, so no `instructions:u` doubling. Arms, after checking each key's current default
(the four last switches were flipped default ON by `85c7a7e7ef`, so the proposal's arm 3 equals arm 2):

| Arm | Environment |
|---|---|
| `pre` | `CRATONVM_JIT_IR_DEOPT_IMAGE_CLAIM=0 CRATONVM_JIT_IR_FRAME_PARAM_LOCALS_ONLY=0 CRATONVM_JIT_IR_FRESH_CLEAR_COVER=0 CRATONVM_JIT_IR_LEAF_FRAME_ELIDE=0` (the pre-j27 defaults) |
| `base` | current defaults (SIZED pair OFF, the four ON) |
| `sized` | `CRATONVM_JIT_IR_DEOPT_IMAGE_SIZED=1 CRATONVM_JIT_IR_SAVE_BANDS_SIZED=1` |
| `img` / `bands` | each SIZED switch alone |

**Engagement** (`CRATONVM_DBG_JITC=1 CRATONVM_DBG=jit-compiled`, `ir-frame-anatomy`):

| Body | pre: frame, clear | base: frame, clear | sized: frame, clear |
|---|---|---|---|
| `Buffer.position(I)Buffer` | 544, 304 | 544, 176 | 352, 128 (saves 72 -> 24, image 256 -> 112) |
| `Buffer.limit(I)Buffer` | 560, 312 | 560, 184 | 368, 136 |
| `ByteBuffer.position(I)Buffer` | 560, 320 | 560, 192 | 368, 144 |
| `ByteBuffer.limit(I)Buffer` | 560, 320 | 560, 192 | 368, 144 |

**Round 1** (9 interleaved reps of all five arms; `J27SmallCallRows -Dj27.timing=true`, median of its own 5
inner reps, then `DecLoop 1000000 5`, last rep), ns/op, median [min-max] of the 9 runs:

| Row | pre | base | sized | img | bands |
|---|---|---|---|---|---|
| wrap | 64.8 [44-95] | 51.7 [42-66] | 53.5 [43-59] | 59.2 [45-70] | 55.6 [41-95] |
| setters | 62.3 [52-164] | 73.5 [56-86] | 63.3 [47-82] | 66.9 [50-96] | 69.1 [48-90] |
| chain | 57.9 [42-307] | 56.4 [38-315] | 53.3 [36-493] | 50.3 [38-284] | 64.5 [37-512] |
| charbuf | 49.4 [39-63] | 41.0 [9-88] | 45.4 [36-62] | 40.8 [9-61] | 44.9 [11-70] |
| iso-decode | 255.4 [210-436] | 289.2 [200-409] | 250.3 [203-462] | 283.4 [179-381] | 300.7 [187-424] |
| sb | 74.0 [66-129] | 78.7 [62-109] | 72.6 [64-118] | 75.7 [60-118] | 83.6 [65-148] |
| DecLoop dec | 290.0 [223-378] | 262.7 [201-420] | 307.4 [209-546] | 250.7 [186-496] | 374.6 [194-670] |
| DecLoop wrap | 95.2 [75-160] | 76.7 [70-139] | 96.1 [72-156] | 86.4 [73-150] | 106.8 [70-171] |
| DecLoop pos | 95.9 [78-148] | 87.9 [79-156] | 106.7 [71-152] | 96.6 [78-130] | 103.2 [71-196] |
| DecLoop sb | 89.8 [75-122] | 91.8 [85-114] | 114.0 [77-185] | 101.3 [76-137] | 131.2 [76-194] |

**Round 2** (15 interleaved reps, base vs sized): wrap 48.6 / 51.4, setters 61.4 / 63.6, chain 53.8 / 59.7,
charbuf 45.1 / 40.4, iso-decode 220.6 / 207.2, sb 72.1 / 68.5, DecLoop dec 302.4 / 260.0, wrap 86.3 / 84.0,
pos 89.4 / 82.8, sb 94.1 / 88.6. Round 1 and round 2 disagree in sign on DecLoop dec/wrap/pos/sb.

**Paired, both rounds (24 pairs, same rep index):**

| Row | sized/base median ratio | sized faster in | pre/base median ratio (9 pairs) | base faster than pre in |
|---|---|---|---|---|
| wrap | 0.959 | 13/24 | 1.174 | 6/9 |
| setters | 0.933 | 13/24 | 0.951 | 4/9 |
| chain | 0.938 | 14/24 | 1.011 | 5/9 |
| charbuf | 0.918 | 16/24 | 1.035 | 6/9 |
| iso-decode | 0.986 | 12/24 | 1.102 | 6/9 |
| sb | 0.939 | 15/24 | 1.033 | 5/9 |
| DecLoop dec | 1.009 | 12/24 | 1.072 | 6/9 |
| DecLoop wrap | 1.028 | 10/24 | 1.152 | 8/9 |
| DecLoop pos | 0.978 | 13/24 | 1.053 | 6/9 |
| DecLoop sb | 1.123 | 11/24 | 0.963 | 4/9 |

HotSpot 25 on the same machine (3 runs): wrap 9.7-14.6, setters 0.7-0.8, chain 0.3-0.4, charbuf 1.1-1.5,
iso-decode 16-21, sb 3.3-4.1; DecLoop dec 7.4-10.4, wrap 5.2-6.4, pos 1.4-1.9, sb 4.0-5.1.

**Decision.** No row moves beyond its spread under `sized`, `img` or `bands` (best sign count 16/24,
p about 0.15 two-sided), so both SIZED switches stay default OFF. The rows sit at 40-80x HotSpot while a
192-byte frame cut buys at most a few ns, so the frame is not the floor here: per the parent page this moves
the hunt to the allocation path (item 4) and the 6.9 KB `wrap` body. The j27 default-ON four are not a
loss here (base beats `pre` on 7 of 10 rows, DecLoop wrap 8/9).

**Facts.** Every `tools/probes/jit/j27` probe, `R21RetainedChurn`, `J27VhGenCardStress -Dj27.rounds=400`,
both modes, arms pre/base/sized: the fact lines of every arm are byte-identical to the base arm. Against
HotSpot 25, the same set differs in every arm, unrelated to the frame switches: `J27DeoptRows`,
`J27MiscRows`, `J27RematRows` match HotSpot run with `-XX:-OmitStackTraceInFastThrow` (HotSpot's
fast-throw drops the message, CratonVM keeps it); `J27Mh2Rows` `mh717-2-*` lacks HotSpot's top frame
`java.lang.Class.getDeclaringClass0`; `J27FfmRows --compatible` dies in boot (`Locale provider adapter
"CLDR" cannot be instantiated`, the fast-boot residual of
[j27-compat-fast-boot-residual-service-loader-and-natives](../j27-compat-fast-boot-residual-service-loader-and-natives-20261028.md))
and matches with `CRATONVM_COMPAT_FAST_BOOT_JIT=0`. **GC battery**: `R21RetainedChurn` and
`J27VhGenCardStress -Dj27.rounds=400` under the default collector, `-XX:+UseG1GC`, `-XX:+UseZGC`,
`-XX:+UseGenerationalGC`, `-Xmx256m`, 2 reps x 3 arms: 48/48 runs exit 0 with HotSpot's fact lines.

## P2. Single-pass OSR guarded pass: bind a `super.m()` inside a guarded body

`plan_osr_door_guarded_splices` calls `resolve_receiver_inline_site(.., direct_bind: None)`, so a guarded
body that contains any call is refused (logged `CalleeUnresolved`, now with a hint). The commonest such
body in NIO is the covariant override `{ super.position(p); return this; }`: an `invokespecial` of the
direct superclass, which JVMS 6.5 selects whatever the receiver (`CRATONVM_JIT_IR_SUPER_SPECIAL_SPLICE`
uses the same argument for the optimizing tier). Design: pass the door's static binder for kind-3 nested
sites whose target is the enclosing body's direct superclass and not `<init>`, so the guarded body keeps
that call as a direct CALL to the compiled `Buffer.position`. Why it is not done here: the existing
comment ("no direct bind inside a guarded body: a call-carrying body is refused, never downgraded") states
a deliberate rule whose reason is not written down at the call site; the binder's pending-compile list and
the guard's miss edge interact with the OSR body's frame layout, which needs a build to check. Value is
limited to loops the single-pass OSR door compiles (a probe's `main`, a test method's 100 M loop when the
optimizing OSR door declines), so do P1 first.

## P3. Not proposed: seeding the `Buffer.<init>` chain's receiver with the `new` class

Tried and reverted on 2026-10-25 (parent page, item 1): it splices all four setter bodies into `wrap` and
`WMin` does not move. Do not redo it without P1's answer.
