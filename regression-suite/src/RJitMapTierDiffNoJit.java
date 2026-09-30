// `RJitMapTierDiff`, run with `--nojit` (via `class_cv_args`).
//
// Not a second test. It is the same 25-shape probe — `RJitMapTierDiff.run`,
// called directly, so the two cannot drift — scheduled a second time under
// the flag that disables the JIT entirely. Same pattern as
// `RClassUnloadSweepGen` re-running `RClassUnloadSweep.unloadedUnderChurn()`
// under `-XX:+UseGenerationalGC`.
//
// Why this exists: `H10-1` built `RJitMapTierDiff` to catch a tier-dependent
// wrong answer (cold != hot, `moved` non-negative), but every arm of the
// corpus schedules it with the JIT on. A vector that only ever runs compiled
// cannot show what its own header calls "Run it BOTH ways": red without
// `--nojit` and green with it is what isolates a divergence to the compiled
// tier rather than to the interpreter or to the shape itself. `H10-1` N2
// named the gap and left it open because a twin needs its own registration
// and name, which is `harness-guard.sh`'s `class_cv_args` and `run.sh`'s
// `CORE_CLASSES` — outside that lane's owned files. This file and the
// matching `class_cv_args` entry close it.
//
// Under `--nojit` every shape stays interpreted for all `ITERS`/`FILL_ITERS`
// iterations, so `moved` is expected to read `-1` on every row here exactly
// as it does in the JIT-on run: the defect `RJitMapTierDiff` was built to
// catch lives in the compiled tier, not the interpreter, so this twin is a
// CONTROL — it must be identical to `RJitMapTierDiff`'s own COLD column on
// every shape. If `RJitMapTierDiffNoJit` ever diverges from HotSpot, the
// defect is not tier-dependent and `RJitMapTierDiff`'s own framing needs
// re-reading.
public class RJitMapTierDiffNoJit {

    public static void main(String[] args) throws Exception {
        RJitMapTierDiff.run("RJitMapTierDiffNoJit");
    }
}
