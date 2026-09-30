/** Build n objects into a local array, CONSUME them, drop the array, collect,
 *  and report what is still held. H2 `TestValueMemory.testType`'s shape: built
 *  and measured inside one method.
 *
 *      cratonvm --java-home "$JDK25" -Xmx2g -cp probes RealDrop 125000
 *      cratonvm --java-home "$JDK25" --nojit -Xmx2g -cp probes RealDrop 125000
 *
 *  The A/B that matters here is the JIT, not the collector. Measured
 *  2026-09-08, `-Xmx2g`:
 *
 *      HotSpot                          before=1785 peak=1273 after=1273
 *      CratonVM --nojit, ZGC            before=1511 peak=1511 after=1511
 *      CratonVM --nojit, generational   before=1341 peak=1341 after=1341
 *      CratonVM --nojit, G1             before=1420 peak=3373 after=1420
 *      CratonVM JIT on, ZGC             before=1511 peak=4441 after=4441
 *      CratonVM JIT on, generational    before=1341 peak=5133 after=4270
 *      CratonVM JIT on, G1              before=1420 peak=4445 after=4445
 *
 *  `after > before` is the only retention. Every `--nojit` row reclaims
 *  completely, every JIT-on row keeps the whole structure: a compiled frame's
 *  dead spill slot still names it, and the conservative JIT-frame scan has no
 *  way to know it is dead.
 *
 *  `peak` is a second, independent reading and it separates the two ways a row
 *  can come back clean. `a` is still in a local slot when `peak` is taken, but
 *  its last use is already past — so a collector with per-bci local liveness
 *  may take the structure before that first reading, and `peak == before` is
 *  what that looks like. HotSpot does (`MethodLiveness` feeds its oop maps) and
 *  so does CratonVM's interpreter
 *  (`runtime::local_liveness::live_locals_mask`). G1's `--nojit` row is the
 *  third case and the reason not to read `peak` alone: it holds the structure
 *  at `peak` (its liveness is region-granular, not per-object) and still frees
 *  every byte of it by `after`.
 *
 *  That difference is the whole of the distance between CratonVM's
 *  `TestValueMemory` Type 0 (2228 KB) and HotSpot's (488) — `--nojit` reads 977
 *  on both collectors. See
 *  `docs/internal/fixed-bugs/h2-testvaluememory-system-gc-retained-every-empty-object-FIXED-20260908.md`.
 *
 *  The CONSUME loop is load-bearing in the same way `ChurnLoop`'s null test is:
 *  a structure whose contents are never read at all can be elided outright, and
 *  then every row reads `before == peak == after` for a reason that has nothing
 *  to do with either liveness or retention. */
public class RealDrop {
    static int sink;

    static long usedKb() {
        System.gc();
        Runtime r = Runtime.getRuntime();
        return (r.totalMemory() - r.freeMemory()) >> 10;
    }

    static void run(int n) {
        long before = usedKb();
        Object[] a = new Object[n];
        for (int i = 0; i < n; i++) {
            a[i] = new Object();
        }
        int h = 0;
        for (int i = 0; i < n; i++) {
            h += System.identityHashCode(a[i]) & 1;
        }
        sink += h;
        long peak = usedKb();
        a = null;
        System.gc();
        System.gc();
        long after = usedKb();
        String verdict;
        if (after > before) {
            verdict = "held past the drop — a frame still names it";
        } else if (peak > before) {
            verdict = "live at peak, fully reclaimed by the drop";
        } else {
            verdict = "already gone at the first reading — precise local liveness";
        }
        System.out.println("before=" + before + " peak=" + peak + " after=" + after
            + " retained=" + (after - before) + "  [" + verdict + "]");
    }

    public static void main(String[] args) {
        run(Integer.parseInt(args[0]));
    }
}
