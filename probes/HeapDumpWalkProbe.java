import java.util.ArrayList;
import java.util.List;

/**
 * Reach `walk_young_objects` through the HPROF heap dump.
 *
 * That walk has two callers. `collect_young_to_old_roots()` in the concurrent
 * old-gen marker needs old gen to stay above 75% AFTER a major cycle, which the
 * generational backend makes hard to arrange: its own Phase-5 major GC runs
 * INSIDE the young collection and clears old below the threshold before
 * `maybe_concurrent_gc` ever asks (measured: old at 97.6% at Phase 5, 55% right
 * after the major, `walk_young=0`).
 *
 * The other caller is `VmHeap::walk_objects()`, which `hprof::dump_heap` uses --
 * and `maybe_dump_heap_on_oom` fires it under `-XX:+HeapDumpOnOutOfMemoryError`
 * with no heap-ratio gymnastics at all. (`jcmd GC.class_histogram` is the third
 * door and stays shut: CratonVM implements no attach listener, so it fails with
 * `java.io.IOException: non existent JVM pid`.)
 *
 * Shape: a live set of EMPTY objects with a scattered half dropped each round.
 * Interleaving live and dead 16-byte all-zero-header objects is what makes the
 * non-moving sweep leave RUNS OF ZEROED SLOTS between survivors -- the shape the
 * walk must not read as a desync. Then exhaust the heap so the OOM fires the
 * dump while from-space still holds those runs.
 */
public class HeapDumpWalkProbe {

    static Object[] retained;
    static Object sink;

    static void churn(int n) {
        for (int i = 0; i < n; i++) {
            Object e = new Object();
            if ((i & 1023) == 0) {
                sink = e;
            }
        }
    }

    public static void main(String[] args) {
        int keep  = args.length > 0 ? Integer.parseInt(args[0]) : 200000;
        int warm  = args.length > 1 ? Integer.parseInt(args[1]) : 8;
        int churn = args.length > 2 ? Integer.parseInt(args[2]) : 300000;
        int chunk = args.length > 3 ? Integer.parseInt(args[3]) : 262144;

        System.out.println("PROBE-START keep=" + keep + " warm=" + warm
                + " churn=" + churn + " chunk=" + chunk);

        retained = new Object[keep];
        for (int i = 0; i < keep; i++) {
            retained[i] = new Object();
        }

        for (int r = 0; r < warm; r++) {
            for (int i = r % 2; i < keep; i += 2) {
                retained[i] = null;
            }
            churn(churn);
            for (int i = 0; i < keep; i++) {
                if (retained[i] == null) {
                    retained[i] = new Object();
                }
            }
            System.out.println("warm=" + r);
        }

        // Exhaust the heap. The dump is written at the allocation failure site,
        // before the OutOfMemoryError reaches Java, so catching it is fine.
        List<byte[]> hog = new ArrayList<>();
        int n = 0;
        try {
            while (true) {
                hog.add(new byte[chunk]);
                n++;
            }
        } catch (OutOfMemoryError oom) {
            hog = null;
            System.out.println("PROBE-OOM after=" + n);
        }
        System.out.println("PROBE-DONE");
    }
}
