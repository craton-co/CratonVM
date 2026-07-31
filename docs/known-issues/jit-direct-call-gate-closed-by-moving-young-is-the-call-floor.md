# The per-call floor is one gate, not call overhead

**Status:** OPEN, root cause confirmed and measured 2026-07-31. The fix is a
GC-root-walk change with a named acceptance gate; it is **not** landed here,
because that acceptance gate cannot currently be run in this checkout (see
*Acceptance*).

## Symptom

A call to a trivial leaf method costs 95-150 ns inside a fully-compiled loop
where HotSpot pays ~0. `probes/CallFloorProbe.java`, 20M iterations, idle host,
ns/op:

| body | CratonVM | HotSpot |
|---|---|---|
| `arith` — the same loop with no call | **1.6** | 0.9 |
| `+ invokestatic` to `static int addOne(int i) { return i + 1; }` | **94.9** | 0.9 |
| `+ invokevirtual` to the same body on a `final` class | **148.9** | 0.8 |
| `+ invokeinterface`, monomorphic | **150.5** | 0.9 |
| `+ String.length()` | 33.2 | 1.3 |

Read the first row first: the loop itself is at **HotSpot parity**. Nothing is
wrong with the compiled code, the OSR entry, or the arithmetic. The entire gap
is the call.

## Root cause

`jit/src/lib.rs::direct_jit_callee_calls_enabled()` returns `false` whenever
`x64::moving_young_enabled()`, and moving-young is **on by default**. With the
gate closed, the JIT plans no `direct_calls` at all, so every
`invokestatic`/`invokespecial`/virtual site in every compiled body falls
through to the generic `jit_invoke_dispatch` helper round trip — on every call,
forever.

Same binary, same run, only the gate moved:

| body | gate closed (default) | `CRATONVM_JIT_DIRECT_CALLEE_CALLS=force` | `CRATONVM_NO_MOVING_YOUNG=1` |
|---|---|---|---|
| `+ invokestatic` | 94.9 | **5.6** | 5.3 |
| `+ invokevirtual` | 148.9 | **37.4** | 38.4 |
| `+ invokeinterface` | 150.5 | **36.1** | 37.7 |
| `+ String.length()` | 33.2 | 32.4 | 34.8 |
| `arith` (control) | 1.7 | 1.3 | 1.2 |

**17x on `invokestatic`, 4x on virtual and interface**, with the call-free
control unmoved. `String.length()` is unmoved too, which is the cross-check
that the lever is specific: it is already served by a call-site intrinsic and
never wanted a direct call.

The gate was re-closed on 2026-07-31 (it had been scoped to
`moving_young_relocates_compiled_frames()` and was measured wrong): with it
open under moving-young, `BasicErrorControllerIntegrationTests` SIGSEGVs on
every run, faulting on a read through a zeroed heap slot — a *reclaimed live
root*. The gate's own comment states the requirement plainly: "Whoever reopens
it needs to make an unguarded callee frame describable to the root scan first,
and should re-run this class 14x as the acceptance gate."

## Why an unguarded callee frame breaks the root scan

The precise root walk keys off two things: a per-thread chain of
`JitFrameChainEntry`, one per interpreter→JIT boundary crossing, each naming
**one** `CompiledMethod` and **one** `exact_rbp`; and a TLS mirror holding the
innermost frame's RBP, which every compiled prologue writes with a single
`mov gs:[disp], rbp`.

A raw JIT→JIT call adds a compiled frame with no chain entry of its own:

1. caller `A` is entered through a guard → entry `E_A{cm: A, exact_rbp: 0}`;
   `A`'s prologue writes the mirror, and `set_top_frame_base` copies it into
   `E_A.exact_rbp`. Correct so far.
2. `A` raw-CALLs `B`. **`B`'s prologue runs the same code**, so the mirror —
   and, under moving-young, `E_A.exact_rbp` — now hold `rbp_B`.
3. `E_A` now says *"method A, at B's frame"*. `scan_one_frame_precise` reads
   `A`'s oop map at `[rbp_B - offset]`, which are `B`'s frame words. `A`'s live
   oops are never reported → reclaimed.

Note the *post-return* half of this is already handled: all five raw-call
emission sites in `x64.rs` call `emit_post_call_rbp_republish`, added for the
IVFKnn corruption. What is missing is a description of the window **while the
callee is executing**.

Note also that the inline MIC/PIC cascade machine-CALLs compiled callees with
no guard on the *default* path, so step 2 is reachable today without the
direct-call gate. Whether something else vetoes precise scanning during that
window was not established here and should be settled first — if not, this is a
live corruption channel independent of the gate.

## Two candidate fixes

**(a) Describe every frame — the real fix.** Give each compiled frame a
one-instruction prologue store of its own identity (the executable buffer's
base address is already known before codegen, so it can be a `mov [rbp-K],
imm64`, and `lookup_jit_code_range` maps it to the `CompiledMethod`). Make
`set_top_frame_base` write `E.exact_rbp` **once** per push — the outermost
frame of that entry — while the mirror keeps tracking the innermost. The walker
then starts at the mirror and follows the saved-RBP chain up to
`E.exact_rbp`, describing each frame with its own map. Zero cost on the call
path, all cost at GC time. This is the shape HotSpot uses and it is what the
gate comment asks for.

**(b) Fall back conservatively.** Mark a `CompiledMethod` that contains raw
call sites; when such a frame is live, have its chain entry additionally
contribute the conservative range `[scanner_sp, exact_rbp)` (which covers every
nested callee frame) and veto moving-young for the cycle, reusing the existing
`unregistered_jit_frame_on_stack` machinery. Much smaller, obviously
over-approximate and therefore safe — but it effectively disables moving-young
whenever a call-bearing compiled frame is live, which is nearly always. That is
trading a GC feature for the 17x, and it is a decision to take with measurements
on real suites, not silently.

Either way `set_top_frame_base`'s write-once change is a prerequisite and is
independently a correctness fix.

## Acceptance

`BasicErrorControllerIntegrationTests` (`module/spring-boot-webmvc`), 14 runs
clean, per the gate comment. **This cannot be run in the current checkout**:
the control run — gate closed, before any change — reports
`tests=26 failed=26` with `NoClassDefFoundError:
org/springframework/boot/SpringApplication`, because
`apps/spring-boot/core/spring-boot/build/libs/spring-boot-4.1.0-SNAPSHOT.jar`
is not built. Rebuild the Spring Boot app tree before attempting the fix;
landing a GC-root-walk change without this gate is exactly what the comment
warns against.

Secondary checks once it passes: `probes/CallFloorProbe.java` for the win,
`BinTreesClassic` d=18 with its checksum, and the Tomcat `util.{buf,collections,
http}` set.

## Reproduction

```powershell
<cratonvm> --java-home <jdk25> -cp <probes> CallFloorProbe 20000000 2000
$env:CRATONVM_JIT_DIRECT_CALLEE_CALLS='force'   # opens the gate in the same binary
$env:CRATONVM_NO_MOVING_YOUNG='1'               # opens it the other way, as a cross-check
```

`CallFloorProbe` runs every body twice — once as one call with a huge loop (OSR
only) and once as many calls with a small loop (compiled by invocation count).
Both columns agreeing, as they do here, is what rules out OSR code quality
before anything else is investigated.

## Not this bug

`docs/known-issues/tomcat/23-charsetcache-pathological-slowdown.md` cites a
"~630 ns marginal cost of an un-inlined call". That figure was measured through
`NativeCallCostProbe`, whose timing loop sits inside a lambda invoked on a
freshly started thread; the shape inflates every rung roughly uniformly. The
figures here — 95 ns for `invokestatic`, 149 ns for virtual — are from a plain
compiled loop and supersede it. The *conclusion* doc 23 draws is unaffected and
in fact sharpened: the control arm makes one call and both cached arms make
two, so closing this gate is what lets the cached arms win.
