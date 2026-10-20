// `RClassUnloadSweep`, run against the GENERATIONAL collector.
//
// Not a second test. It is the same probe — `RClassUnloadSweep`'s own
// `unloadedUnderChurn()`, called directly, so the two cannot drift — scheduled
// a second time with `-XX:+UseGenerationalGC` supplied by `run.sh`'s
// `class_cv_args`. It exists because the collector is a variable this suite
// otherwise never moves.
//
// Why that matters here specifically. `TestDefaultInstanceManager.
// testClassUnloading` has now failed FOUR times, and its own writeup ends with
// "Do not close this without a regression pin". A pin was added
// (`RClassUnloadSweep`) — and it ran only on whatever collector happened to be
// the default, which changed to ZGC on 2026-08-10 without any of this being
// re-checked. The last surviving arm of that bug was Generational-only, so the
// scheduled vector could not have caught it, and did not: measured on
// 2026-08-13, the pre-strengthening vector printed `unloaded=true` under
// `-XX:+UseGenerationalGC` with the young sweep's empty-object-run recovery
// deliberately switched off, i.e. it passed on a VM carrying the exact defect
// it was written to pin.
//
// Two changes together close that. `RClassUnloadSweep.emptyChurn` supplies the
// allocation SHAPE the defect needs (its own comment says why `byte[128]`
// cannot), and this file supplies the COLLECTOR. Neither is sufficient alone,
// and the A/B in `emptyChurn`'s comment is the measurement that says so.
//
// The HotSpot side of the diff deliberately does NOT receive the flag —
// `run.sh` passes `class_cv_args` through `$cvextra`, which is CratonVM-only,
// and `-XX:+UseGenerationalGC` is a CratonVM spelling a real JVM would reject.
// HotSpot therefore runs its own default collector and still clears the
// reference, which is exactly the oracle this vector wants: "a real JVM unloads
// this class" is the claim, not "a real JVM unloads it under a named
// collector".

public class RClassUnloadSweepGen {

    public static void main(String[] args) throws Exception {
        boolean cleared = RClassUnloadSweep.unloadedUnderChurn();

        // Same contract as `RClassUnloadSweep`: DIFF-ONLY, on a `CK ` line.
        // Whether a weak reference has actually been cleared is a GC-policy
        // outcome rather than a language guarantee, so a local assertion would
        // turn a legitimate collector configuration into a red; HotSpot clears
        // it within the budget, so a CratonVM printing `false` is a real
        // divergence and the cross-VM diff is the right instrument.
        System.out.println("CK RClassUnloadSweepGen payload.class.unloaded=" + cleared);
        System.out.println("PASS RClassUnloadSweepGen");
    }
}
