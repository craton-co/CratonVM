/**
 * Regression: a `System.gc()` that runs a MAJOR collection inside a moving
 * young cycle must not reclaim the backing state of an overlay-backed
 * collection.
 *
 * CratonVM keeps `LinkedHashMap` / `LinkedHashSet` / `LinkedList` / `TreeMap` /
 * `TreeSet` state in process-global Rust side tables, reached by the collector
 * through `external_roots`. `System.gc()` sets `major_gc_requested()`, and that
 * flag makes `native_roots::scan_collection_overlays` SKIP the precise overlay
 * root scan — by design, because the major GC's owner walk is supposed to cover
 * it instead. The owner walk reads the side tables, and the moving young phase
 * that runs first leaves them holding PRE-copy addresses until the VM's post-GC
 * remap, which happens after the major GC has already swept. Anything the cycle
 * promoted is therefore unmarked and freed while the overlay holds the only
 * reference to it.
 *
 * The shape that matters is: an overlay-backed collection that survives long
 * enough to be PROMOTED, whose promoting cycle is also a `System.gc()`. So the
 * loop below keeps every collection alive across many explicit collections
 * rather than dropping them, and re-verifies the OLD ones each round — a fresh
 * map that never got promoted cannot express the bug.
 *
 * Deterministic output; the runner diffs it against HotSpot.
 *
 *   cratonvm --nojit --Xmx 256m ROverlaySystemGcStress
 *
 * KNOWN FAILING with JIT ON against a real JDK, and deliberately NOT wired into
 * `run.sh`'s `CORE_CLASSES` for that reason — it would make the suite red on a
 * defect that has no accepted fix yet:
 *
 *     AssertionError: tm size 0 != 24 (bundle 0)
 *
 * and on `dev`'s own tip, harder:
 *
 *     ClassCastException: class java.lang.Object cannot be cast to Bundle
 *
 * That is defect 4 in
 * `docs/known-issues/hibernate/map-resize-unpinned-chain-cursors-nojit-segv-20260731.md`:
 * the in-place old-gen sweep returns a LIVE promoted object's block to the free
 * list, because selective promotion leaves the roots on their pre-promotion
 * young addresses and the sweep's seed loop drops them as not-old-gen. Run it
 * by hand when working on that:
 *
 *   cratonvm --java-home <jdk> -cp build ROverlaySystemGcStress        # fails
 *   CRATONVM_OLD_SWEEP_JIT=0 cratonvm --java-home <jdk> -cp build ...  # passes
 */
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.LinkedHashSet;
import java.util.LinkedList;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;
import java.util.TreeSet;

public class ROverlaySystemGcStress {
    static int checks = 0;

    static void check(boolean c, String m) {
        checks++;
        if (!c) throw new AssertionError(m);
    }

    /** One generation's worth of overlay-backed collections, all cross-checked. */
    static final class Bundle {
        final int id;
        final LinkedHashMap<String, String> lhm = new LinkedHashMap<>();
        final LinkedHashSet<String> lhs = new LinkedHashSet<>();
        final LinkedList<String> ll = new LinkedList<>();
        final TreeMap<String, String> tm = new TreeMap<>();
        final TreeSet<String> ts = new TreeSet<>();

        Bundle(int id, int width) {
            this.id = id;
            for (int i = 0; i < width; i++) {
                String k = "k" + id + "." + i;
                lhm.put(k, "v" + id + "." + i);
                lhs.add(k);
                ll.add(k);
                tm.put(k, "v" + id + "." + i);
                ts.add(k);
            }
        }

        void verify(int width) {
            check(lhm.size() == width, "lhm size " + lhm.size() + " != " + width + " (bundle " + id + ")");
            check(lhs.size() == width, "lhs size " + lhs.size() + " != " + width + " (bundle " + id + ")");
            check(ll.size() == width, "ll size " + ll.size() + " != " + width + " (bundle " + id + ")");
            check(tm.size() == width, "tm size " + tm.size() + " != " + width + " (bundle " + id + ")");
            check(ts.size() == width, "ts size " + ts.size() + " != " + width + " (bundle " + id + ")");
            for (int i = 0; i < width; i++) {
                String k = "k" + id + "." + i;
                String v = "v" + id + "." + i;
                check(v.equals(lhm.get(k)), "lhm lost " + k + " -> " + lhm.get(k));
                check(lhs.contains(k), "lhs lost " + k);
                check(v.equals(tm.get(k)), "tm lost " + k + " -> " + tm.get(k));
                check(ts.contains(k), "ts lost " + k);
            }
            // Insertion order is the whole point of the overlay for LHM/LHS/LL.
            int i = 0;
            for (Map.Entry<String, String> e : lhm.entrySet()) {
                check(e.getKey().equals("k" + id + "." + i), "lhm order broke at " + i + ": " + e.getKey());
                i++;
            }
            check(i == width, "lhm iteration yielded " + i + " of " + width);
            i = 0;
            for (String k : lhs) {
                check(k.equals("k" + id + "." + i), "lhs order broke at " + i + ": " + k);
                i++;
            }
            check(i == width, "lhs iteration yielded " + i + " of " + width);
            check(ll.getFirst().equals("k" + id + ".0"), "ll head broke: " + ll.getFirst());
            check(ll.getLast().equals("k" + id + "." + (width - 1)), "ll tail broke: " + ll.getLast());
        }
    }

    public static void main(String[] args) {
        final int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 40;
        final int width = args.length > 1 ? Integer.parseInt(args[1]) : 24;
        // `System.gc()` diverts CratonVM's generational collector to the
        // NON-MOVING young sweep (`explicit_full_gc` in `collect_garbage_inner`),
        // so the explicit-collection arm cannot reach the moving Cheney path at
        // all. Pass `0` to drive collections by allocation pressure instead —
        // the same path `--nojit` application code takes.
        final boolean explicitGc = args.length <= 2 || !"0".equals(args[2]);
        final int churnPerRound = explicitGc ? 512 : 20000;
        // Retained ballast, in 4 KiB blocks. A minor GC alone cannot express the
        // same-cycle-major-GC defect this probe exists for: Phase 5 only
        // compacts once old gen crosses 75 % occupancy, and at any ordinary heap
        // size the bundles above never get old gen anywhere near that
        // (`CRATONVM_DBG_MIRRORPIN=1` reports `will_run_major=false` for every
        // cycle). Default 0 so the suite run stays quick; pass a count with a
        // small -Xmx to actually reach a compacting cycle.
        final int ballastBlocks = args.length > 3 ? Integer.parseInt(args[3]) : 0;

        // Every bundle stays reachable for the whole run, so each explicit
        // collection ages it one step closer to promotion — the promoting
        // cycle is the one that matters, and it has to also be a System.gc().
        List<Bundle> live = new ArrayList<>();
        List<byte[]> ballast = new ArrayList<>();
        for (int i = 0; i < ballastBlocks; i++) {
            byte[] block = new byte[4096];
            block[0] = (byte) i;
            ballast.add(block);
        }
        long churn = ballast.size();
        for (int r = 0; r < rounds; r++) {
            live.add(new Bundle(r, width));

            // Ordinary allocation between collections, so old gen fills and the
            // survival rate stays high enough for the promote-on-pressure path.
            for (int i = 0; i < churnPerRound; i++) {
                churn += new StringBuilder("churn").append(r).append('.').append(i).toString().length();
            }

            if (explicitGc) {
                System.gc();
            }

            // Re-verify EVERY bundle, not just the newest: the damage lands on
            // the ones old enough to have been promoted.
            for (Bundle b : live) {
                b.verify(width);
            }
        }

        System.out.println("CK rounds=" + rounds + " width=" + width
                + " bundles=" + live.size() + " ballast=" + ballast.size()
                + " churn=" + (churn > 0));
        System.out.println("PASS ROverlaySystemGcStress (" + checks + " checks)");
    }
}
