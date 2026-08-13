/**
 * Push the OLD generation hard, which is what the last two unreached walks
 * need: `fixup_young_old_refs` runs only when a compaction actually MOVES
 * something (`compact_map` non-empty), and `walk_young_objects` is reached from
 * the concurrent old-gen marker, which is gated on `old_gen_needs_gc()`.
 *
 * Shape: build a large long-lived population so it ages and promotes, then drop
 * a scattered half of it so the old generation is fragmented rather than merely
 * full — a compaction with nothing to slide produces an empty map and the fixup
 * never runs. Empty objects are threaded through the payloads so the zero-run
 * shape is present in young at the same time.
 */
public class OldGenProbe {

    static final class Blob {
        Object empty;
        Blob link;
        byte[] bulk;      // gives old gen real mass to fragment
        int tag;
        Blob(Object e, Blob l, int bytes, int t) {
            empty = e; link = l; bulk = new byte[bytes]; tag = t;
        }
    }

    static Blob[] tenured = new Blob[1 << 14];

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 12;
        int churn  = args.length > 1 ? Integer.parseInt(args[1]) : 120_000;

        for (int round = 0; round < rounds; round++) {
            // (1) fill: long-lived blobs, each holding an empty object.
            for (int i = 0; i < tenured.length; i++) {
                if (tenured[i] == null) {
                    tenured[i] = new Blob(new Object(), tenured[(i * 7) & (tenured.length - 1)],
                                          64 + (i & 255), i);
                }
            }
            // (2) young churn: dead empty objects, which make the zero runs.
            for (int i = 0; i < churn; i++) {
                Object empty = new Object();
                if ((i & 63) == 0) {
                    int slot = (i >>> 6) & (tenured.length - 1);
                    if (tenured[slot] != null) tenured[slot].empty = empty;
                }
            }
            // (3) fragment: drop a scattered third so compaction has gaps to
            // close AND live neighbours to slide, rather than a clean tail.
            for (int i = round % 3; i < tenured.length; i += 3) {
                tenured[i] = null;
            }
            System.gc();
            int live = 0;
            for (Blob b : tenured) if (b != null) live++;
            System.out.println("round=" + round + " liveBlobs=" + live);
        }
        int live = 0;
        for (Blob b : tenured) if (b != null) live++;
        System.out.println("PROBE-DONE liveBlobs=" + live);
    }
}
