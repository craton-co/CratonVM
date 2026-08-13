/**
 * Drive the OLD generation past its 75% major-GC threshold by ALLOCATION
 * ALONE. That single condition is the prerequisite for both of the young-walk
 * sites no suite workload reaches:
 *
 *   - `walk_young_objects` is called from `collect_young_to_old_roots()` in the
 *     concurrent old-gen marker, which `maybe_concurrent_gc` starts after a
 *     minor GC when `old_gen_needs_gc()` — old used >= 75% of old capacity.
 *   - `fixup_young_old_refs` runs after an old-gen compaction that MOVED
 *     something. Compaction lives on the MOVING young path's `major_gc`, gated
 *     on `old_gen.used() >= capacity * 75/100 || major_requested`.
 *
 * `System.gc()` is deliberately NOT used, and that is the correction over the
 * first two attempts. An explicit full GC takes the non-moving young path
 * (`nonmoving-explicit-full-gc`), and that path reaches old gen through
 * `sweep_old_gen_non_moving`, which calls `old_gen_gc(.., compact = false, ..)`
 * with the flag hardcoded. So no number of `System.gc()` calls can ever produce
 * a non-empty `compact_map` — measured: 6 majors, `fixup_yo=0`, and
 * `oldgen_coalesce calls=6` showing the in-place arm took every one.
 *
 * Sizing is the other lever: `-Xmx N` splits into young_semi = N/4 and old =
 * N/2, so 75% of old is ~48 MB at `-Xmx 128m` rather than a gigabyte.
 *
 * Shape: hold a large live set, and each round REPLACE a rotating third of it.
 * The replaced nodes have already been promoted, so they become dead objects
 * sitting in old gen between live ones — which is both what pushes old past
 * 75% and what gives a sliding compactor something to slide over. Nothing frees
 * them until the major cycle fires, which is exactly the event under test.
 */
public class OldGenFillProbe {

    static final class Node {
        Object empty;   // the all-zero-header shape, in young beside the walk
        byte[] payload; // mass, so old gen fills in tens of MB not tens of GB
        Node next;      // a real old->old edge for the compactor to rewrite
        int tag;
        Node(int tag, int bytes) {
            this.tag = tag;
            this.payload = new byte[bytes];
            this.empty = new Object();
        }
    }

    static Node[] retained;
    static Object sink;

    static int countLive() {
        int n = 0;
        for (Node x : retained) {
            if (x != null) {
                n++;
            }
        }
        return n;
    }

    /** Allocation pressure -- this, not System.gc(), is what ages survivors. */
    static void churn(int n) {
        for (int i = 0; i < n; i++) {
            Object e = new Object();
            if ((i & 1023) == 0) {
                sink = e;
            }
        }
    }

    public static void main(String[] args) {
        int slots   = args.length > 0 ? Integer.parseInt(args[0]) : 30000;
        int payload = args.length > 1 ? Integer.parseInt(args[1]) : 1024;
        int rounds  = args.length > 2 ? Integer.parseInt(args[2]) : 30;
        int churn   = args.length > 3 ? Integer.parseInt(args[3]) : 200000;
        int oom     = args.length > 4 ? Integer.parseInt(args[4]) : 0;

        retained = new Node[slots];
        System.out.println("PROBE-START slots=" + slots + " payload=" + payload
                + " rounds=" + rounds + " churn=" + churn + " oom=" + oom);

        for (int round = 0; round < rounds; round++) {
            // Replace a rotating third. The outgoing nodes are already in old
            // gen; dropping them leaves dead blocks BETWEEN live ones, and
            // nothing reclaims those until old crosses 75%.
            for (int i = round % 3; i < slots; i += 3) {
                retained[i] = new Node(i, payload);
            }
            for (int i = 0; i < slots; i++) {
                if (retained[i] == null) {
                    retained[i] = new Node(i, payload);
                }
            }
            // Real edges for the compactor's Phase 2 to rewrite.
            for (int i = 0; i < slots; i++) {
                retained[i].next = retained[(i * 7 + 3) % slots];
            }
            churn(churn);
            if ((round & 3) == 0 || round == rounds - 1) {
                System.out.println("round=" + round + " live=" + countLive());
            }
        }

        // Optional tail: exhaust the heap so `-XX:+HeapDumpOnOutOfMemoryError`
        // fires `hprof::dump_heap` -> `VmHeap::walk_objects` ->
        // `walk_young_objects`. That is the reachable door for the third walk;
        // the concurrent old-gen marker's door stays shut because the
        // generational Phase-5 major runs INSIDE the young collection and
        // clears old below 75% before `maybe_concurrent_gc` asks.
        //
        // It has to be THIS workload rather than a plain allocation loop: a
        // bare `new Object()` churn keeps no JIT frame live at a safepoint, so
        // every cycle takes the MOVING path (`moving-no-jit-frames-live`) and
        // from-space is dense with no zero runs to misread. This one leaves
        // `sp_sweeps > 0` and `zero_empty_runs > 0`.
        //
        // The OOM has to land IMMEDIATELY after a `System.gc()`, and that is
        // the whole trick. An explicit full GC is the one deterministic way to
        // select the non-moving young sweep (`nonmoving-explicit-full-gc`),
        // and only that sweep leaves runs of zeroed dead empty objects in
        // from-space. A gradual `hog.add(new byte[chunk])` loop instead runs
        // many more collections on the way down, each of them MOVING, and
        // hands the dump a dense from-space with nothing to misread. So: sweep,
        // then ask for an array no heap of this size can ever serve, which
        // fails in one step without a moving cycle in between.
        if (oom != 0) {
            System.gc();
            System.gc();
            try {
                sink = new byte[Integer.MAX_VALUE - 8];
            } catch (OutOfMemoryError e) {
                System.out.println("PROBE-OOM " + e.getClass().getName());
            }
        }

        System.out.println("PROBE-DONE live=" + countLive());
    }
}
