# A generic `invokedynamic` answers the `b = true` result for `b = false` on worker threads under the JIT

**Status: FIX BUILT (unbuilt, not run), behind `CRATONVM_INDY_GENERIC_KEEP_PRIMITIVE_Z` (HotSpot
parity, default ON, `=0` restores the old boxing). Filed 2026-12-24 by interpreter round i6 wave
134, lane L2c.** Move this page to `docs/internal/` once the run below passes.

## Symptom (MEASURED by the orchestrator)

`tools/probes/interp/L2b/L2bW133GenericIndyThreadRow.java`, with the JIT on, in both `--jdk-only`
and `--compatible`. The result is the same on every build from w124 (`869ab6e395`) to w133
(`baab0853b4`), with `CRATONVM_INDY_GENERIC_THREAD_ROW` unset and with `=1`:

```text
phase1 threads=4 mismatches=1196   (HotSpot: 0)
phase2 threads=4 mismatches=1200 walk=390   (HotSpot: 0)
```

Every other line equals HotSpot:
* `linked bootstraps=82 walk=860 both=pair#80:3|pair#81:3`;
* `after-gc walk=860 both=pair#80:8|pair#81:8`;
* `bootstraps=82`.

With `--nojit`, every line equals HotSpot.

## What the numbers say (by arithmetic over the probe's worker loop)

Each worker runs 300 iterations with `i = (7n + seed) % 1000` and `b = ((n + seed) & 1) == 0`.
Each iteration makes two checks, `walk` (a sum of string LENGTHS) and `both` (`equals`):
* exactly 150 iterations per thread have `b = false`;
* none of those has `i = 0` for `n < 300`, so `-i` always adds one character.

`1200 = 4 threads x 150 b=false iterations x 2 checks`. Every `b = false` iteration fails BOTH
checks, and no `b = true` iteration fails either one.

`1196` is four fewer, which is two iterations passing both checks. Those are the `n = 0`
iterations of seeds 1 and 3, the only `b = false` first iterations. They ran before the compiled
code involved was in place. In phase 2 everything is already compiled, so nothing passes.

Every main-thread line of the probe passes `b = true` (`walk(3,true)`, `both(3,true)`,
`walk(8,true)`, `both(8,true)`, `walk(1,true)`). That is why they all equal HotSpot.

So the defect is NOT the suspected per-entry binding:
* the main thread's `both` answers `pair#80 | pair#81`, so the two instructions are linked
  separately;
* `walk=860` / `walk=390` are right across all 80 sites, so there is no aliasing in the 64-slot
  table;
* `walk=390` after `setTarget` is right, so the retarget is seen.

The read is that some JIT-only layer turns the `b = false` result into a string whose LENGTH
differs, most likely `prefix + i` in place of `prefix + -i`, i.e. the `b = true` arm. Both the
80-site `walk` and the two-site `both` go through that layer.

The target is
`tag(String prefix, Object o, int i, boolean b) { return prefix + (b ? i : -i); }`, behind a
`MutableCallSite` over `insertArguments(tag, 0, prefix)`. Every site's profile, and `tag`'s own,
saw only `b = true` before the workers started: main linked and ran each site once with `true`.

## Measured on the diagnostic (orchestrator, w133 `baab0853b4`, `--jdk-only`, JIT on)

```text
direct mismatches=0
mh mismatches=0
indy-main walk=1500 both=1500 flag=1500 ; first-mismatch walk(7,false)=860 want 940
indy-const walk=2997 both=2997 flag=3000
indy-one walk=1500 both=1500 flag=1500
indy-four walk=600 both=600 flag=600
single walk=860 both=pair#80:7|pair#81:7 flag=flag#82:7/true
```

With `--nojit`, every count is 0 and `single` equals HotSpot.

The `flag` target RECEIVES `true` for a `false` argument on every indy path, including from an
interpreted caller (`single`). Calling `tag` directly and calling the same bound handle through
`invokeExact` are both right.

None of these changed anything:
* `CRATONVM_JIT_IR_PRUNE_NEVER_TAKEN=0` with `CRATONVM_JIT_PROF_UNSTABLE_IF_REASON=0`;
* `CRATONVM_JIT_INDY_LINKED_CALL_SITE=0` with `CRATONVM_JIT_INDY_LINKED_DIRECT=0`;
* `CRATONVM_INDY_GENERIC_DIRECT_INVOKE=0`;
* `CRATONVM_JIT_CONCAT_INLINE=0` with `CRATONVM_JIT_CONCAT_LATIN1_FAST=0`;
* `CRATONVM_JIT_SP_ZERO_DEAD_ARG_SOURCES=0`.

## Cause (by reading, consistent with every measured row)

`invoke_generic_call_site_with` (`vm/src/runtime/invokedynamic.rs`) is the one function the
interpreter's generic indy executor and the compiled bridge share. It replaces every `Z` operand
with the canonical `Boolean.TRUE` / `Boolean.FALSE` box before `MethodHandle.invoke`. That box
exists for Groovy's `Object[]` bridge. For a target whose own `type()` says `boolean`, the call is
at the handle's own type, so nothing converts the box back. It travels to the leaf as a reference
in a `boolean` slot:
* an INTERPRETED target receives the right value. `--nojit` is right, so some step on the
  interpreted entry converts the box by descriptor; the lane did not locate that step;
* the interpreter-to-compiled door (`execute_jit_call`: `decode_arg_kind_aware`, then
  `jit_abi_arg_bits`) passes the reference's ADDRESS as the `boolean` word, and the compiled
  `ifeq` sees a non-zero value, so `false` reads as `true`.

So the error starts as soon as the target (`tag` / `tagFlag`) is compiled, whoever the caller is.
The `invokeExact` path pushes the `int` and is right. HotSpot boxes nothing at the handle's own
type.

## Fix

`CRATONVM_INDY_GENERIC_KEEP_PRIMITIVE_Z`, read per VM as `VmConfig::indy_generic_keep_primitive_z`.
* **What changes.** Before the boxing loop, the target handle's own `type()` is read (pure reads:
  its `type` field, then `method_type_object_descriptor`), by `target_handle_param_types`. A `Z`
  operand whose position in that type is the primitive `boolean` keeps its `int`.
* **What stays.** An `Object`, `Boolean` or collector position, an unreadable type, and an arity
  mismatch are boxed exactly as before. So are Groovy's selectors, which have `Object`
  parameters.
* **`=0`** restores the old boxing everywhere.

## Candidate layers considered before the measurement (kept for the record)

1. **Compiled `tag`'s never-taken `ifeq`.** Its profile is `b = true` only (82 calls), so the
   optimizing tier prunes the taken edge (`IrBuilder::prune_never_taken_branch`,
   `CRATONVM_JIT_IR_PRUNE_NEVER_TAKEN`, `DeoptReason::UnstableIf`). A wrong frame state or resume
   on that trap, for a frame entered from the VM's `MethodHandle` invoke rather than from
   compiled code, would answer the fall-through arm. The withdrawal after one charge should then
   make the error transient, though, and the measured error is permanent.
2. **The `Z` operand on the compiled side.** Compiled `walk` / `both` stage `iload_3` into the
   bridge buffer. `execute_jit_indy_generic_raw` decodes it as `Value::Int(raw as i32)`, and
   `invoke_generic_call_site_with` boxes it with `canonical_boolean(shared, v != 0)`. A raw word
   that is not exactly 0 for `false` (for example a compiled caller's `b` with junk above the low
   bit) becomes `Boolean.TRUE`, which is exactly the symptom. The interpreter pops a typed
   `Value::Int` and cannot show it.
3. **The generic invoke of a bound (non-direct) target from the bridge.** The paths are:
   * `CRATONVM_INDY_GENERIC_DIRECT_INVOKE`'s registry callback, or the
     `MethodHandle.invoke` walk;
   * the `poly_call_site::arm` descriptor;
   * the unboxing of the boxed `Boolean` into `tag`'s `boolean` when `tag` is compiled, which
     differs from the interpreted entry.
4. **The inline concat of `prefix + int`** (`CRATONVM_JIT_CONCAT_INLINE`, ON). Reading
   `x64/concat_emit.rs`, the sign is handled (`emit_decimal`: `JNS`, `'-'`, `NEG`), and so it is
   in `concat_latin1::push_decimal`. It is listed only because it is the one compiled step that
   renders `-i`.

Ruled out by reading: the intern table and every bridge key are per INSTRUCTION. That covers
`interned_jit_indy_bridge_site`, `make_jit_indy_linked_site_from_parts`,
`JitIndyGenericSite::instruction_key`, `indy_instruction_key`, the `GENERIC_INDY_SITES` key, and
`ThreadInstructionSites` (whose key compares the class, the CP index, the pc and the method). The
IR mutable guard (`try_indy_mutable_guarded_call`) is planned only for a bound direct leaf of
exactly the site's type, and `insertArguments(tag, 0, ..)` is not one: `mh_compiled_bind_target`
requires `own_type == site`, and the leaf's argument count must equal the site's.

## The run that pins the layer

The lane added `tools/probes/interp/L2c/L2cW134GenericIndyFalseArm.java`. Its HotSpot block was
MEASURED on 25.0.3 under default flags and `-Xint` (both the same). It warms every path with
`b = true`, then alternates, single-threaded first, and counts the three checks apart:

| Section | What it calls | Layer it tests |
|---|---|---|
| `direct` | a compiled loop calling `tag` directly | 1, or a plain miscompile of `b ? i : -i` |
| `mh` | a compiled loop calling the bound handle through `invokeExact` | 3, without indy |
| `indy-main` | a compiled loop on main calling `walk` / `both` / `flag` | 2 or 3, single-threaded |
| `indy-const` | as `indy-main`, with a literal `false` | as `indy-main` |
| `indy-one` | `indy-main`'s loop on one worker thread | as `indy-main` |
| `indy-four` | L2b's four-thread shape | the original symptom |
| `single` | one call each with `false` | the interpreted-caller control |

The `flag` site's target answers `prefix + i + "/" + b`, i.e. the boolean the TARGET received.

A failing section prints a `first-mismatch` line naming the wrong answer. HotSpot prints none.

```text
javac -d <dir> tools/probes/interp/L2c/L2cW134GenericIndyFalseArm.java
for m in --jdk-only --compatible; do for j in "" --nojit; do
  cratonvm $m $j -cp <dir> L2cW134GenericIndyFalseArm
done; done
# then, JIT on, one arm per run, to bisect the layer the first run names:
#   CRATONVM_JIT_IR_PRUNE_NEVER_TAKEN=0  CRATONVM_JIT_PROF_UNSTABLE_IF_REASON=0   (candidate 1)
#   CRATONVM_JIT_INDY_LINKED_CALL_SITE=0  CRATONVM_JIT_INDY_LINKED_DIRECT=0       (candidate 2/3)
#   CRATONVM_INDY_GENERIC_DIRECT_INVOKE=0                                         (candidate 3)
#   CRATONVM_JIT_CONCAT_INLINE=0  CRATONVM_JIT_CONCAT_LATIN1_FAST=0               (candidate 4)
#   CRATONVM_JIT_SP_ZERO_DEAD_ARG_SOURCES=0
```

How to read the result:
* `direct` fails: candidate 1, or a plain miscompile of `b ? i : -i`.
* `mh` fails and `direct` passes: candidate 3 without indy.
* Only the `indy-*` rows fail, and `flag` shows `/true` for a `false` call: the boolean is lost
  between the compiled `walk` and the target (candidate 2 or 3).
* `flag` passes but `walk` / `both` fail: the target's own `-i` (candidate 1 or 4).

## Pass criteria for the fix

All of these, in both modes, JIT and `--nojit`:
* `L2bW133GenericIndyThreadRow` equals its HotSpot block, with `CRATONVM_INDY_GENERIC_THREAD_ROW`
  unset and `=1`;
* `L2cW134GenericIndyFalseArm` equals its HotSpot block;
* the regression set is unchanged: `tools/probes/interp/L4/L4W117GenericIndyBench.java`
  (behaviour lines), `tools/probes/interp/L6/GenericIndyLinkProbe.java`,
  `tools/probes/interp/L6/GenericIndyStaticArgsProbe.java`,
  `tools/probes/interp/L4/L4W32IndyTargetType.java`,
  `tools/probes/interp/L4/L4W101IndyPrimitiveBootstrapResult.java`.

All of the above, with `CRATONVM_INDY_GENERIC_KEEP_PRIMITIVE_Z` unset (the fix). The `=0` arm
must reproduce the measured defect (it is the escape hatch, not a pass arm).

Groovy is the population the original boxing was for. Run the Groovy / Spring Boot Thymeleaf
witness named in the boxing comment (`ThymeleafServletAutoConfigurationTests`, LayoutDialect's
`DecorateProcessor`) with the switch at its default.
