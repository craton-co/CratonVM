/** Allocate n immediately-dead `Object`s per round, store nothing, `System.gc()`,
 *  and report the heap the VM says is in use.
 *
 *  This is the minimal reproducer for
 *  `h2-testvaluememory-system-gc-retained-every-empty-object-FIXED-20260908`.
 *  It has no collections, no dropped references, no H2 and no value types —
 *  just allocation and an explicit collection. Before that fix, a field-less
 *  object reached the heap as sixteen zero bytes (`java/lang/Object` is
 *  `ClassId(0)`, `shape` is 0, and a neutral mark word was 0), which the young
 *  non-moving sweep cannot tell from reclaimed, zeroed arena space — so under
 *  `-XX:+UseGenerationalGC`, which every `System.gc()` routes to that sweep,
 *  `usedKb` climbed ~2.1 MB a round, monotonically, and 40 rounds did not
 *  finish inside 300 s. Now it plateaus and 40 rounds take ~1.4 s.
 *
 *      cratonvm --java-home "$JDK25" -XX:+UseGenerationalGC -Xmx256m \
 *          -cp probes ChurnLoop 40 125000
 *
 *  Run it without `-XX:+UseGenerationalGC` for the control.
 *
 *  The `if (o == null)` is LOAD-BEARING. Without a use, the allocation is a
 *  candidate for elimination and the probe measures nothing — which is how a VM
 *  that elides the workload comes back looking like a VM that collects it. */
public class ChurnLoop {
    static long usedKb() {
        System.gc();
        Runtime r = Runtime.getRuntime();
        return (r.totalMemory() - r.freeMemory()) >> 10;
    }

    public static void main(String[] args) {
        int rounds = Integer.parseInt(args[0]);
        int n = Integer.parseInt(args[1]);
        for (int k = 0; k < rounds; k++) {
            for (int i = 0; i < n; i++) {
                Object o = new Object();
                if (o == null) {
                    System.out.print("");
                }
            }
            if (k % 5 == 0 || k == rounds - 1) {
                System.out.println("round=" + k + " usedKb=" + usedKb()
                    + " totalKb=" + (Runtime.getRuntime().totalMemory() >> 10));
            }
        }
        System.out.println("SURVIVED all " + rounds + " rounds");
    }
}
