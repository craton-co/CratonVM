import java.util.LinkedHashMap;
import java.util.ArrayList;
import java.util.List;

// WORKER-5-NOTE-10 N2: does gc_prune_dead_collection_overlays ever get
// skipped for a whole collection, or does collection_count() just count a
// combined minor+major cycle as 2? This probe forces several minor-only
// cycles (small dead LinkedHashMaps, no promotion pressure) followed by a
// promotion-heavy phase (long-lived arrays pushed into old gen) so that at
// least one cycle should set major_ran=true. Correlate the host's
// [overlay-prune] collection_count= lines against the native-collections
// [DBG_MIRRORPIN] CALLED lines: every CALLED must be preceded by exactly one
// collection_count line, and a count that jumps by 2 between two consecutive
// CALLED lines is one physical cycle (a combined minor+major), not a missed
// one.
public class MirrorPinCoalesceProbe {
    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        // Phase 1: minor-only pressure. Small dead maps, more garbage, kept
        // well under the old-gen promotion threshold.
        for (int round = 0; round < 60; round++) {
            for (int i = 0; i < 500; i++) {
                LinkedHashMap<Integer, Integer> m = new LinkedHashMap<>();
                m.put(i, i * 2);
                m.put(i + 1, i * 3);
                sink = m; // touched, then dropped next iteration
            }
            byte[] junk = new byte[256 * 1024];
            sink = junk;
        }
        System.out.println("phase1 done");

        // Phase 2: promotion pressure -- keep growing a list of long-lived
        // arrays so old gen crosses the major_ran threshold repeatedly, while
        // still creating dead LinkedHashMaps each round so the prune has real
        // work every cycle.
        List<byte[]> longLived = new ArrayList<>();
        for (int round = 0; round < 300; round++) {
            for (int i = 0; i < 500; i++) {
                LinkedHashMap<Integer, Integer> m = new LinkedHashMap<>();
                m.put(i, i);
                sink = m;
            }
            longLived.add(new byte[512 * 1024]);
            byte[] junk = new byte[256 * 1024];
            sink = junk;
            if (round % 40 == 39) {
                // periodically drop some old growth so we don't just OOM,
                // but keep enough resident to hold old gen near the
                // promotion threshold across many cycles.
                for (int k = 0; k < 20 && !longLived.isEmpty(); k++) {
                    longLived.remove(0);
                }
            }
        }
        System.out.println("phase2 done, longLived=" + longLived.size());
    }
}
