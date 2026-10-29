/**
 * Regression: the in-place old-gen sweep must not free an object this same
 * cycle just PROMOTED (defect 4).
 *
 * `sweep_young_non_moving` copies each promoted survivor into old gen and
 * clears its mark bit, so the copy arrives UNMARKED — and
 * `sweep_old_gen_non_moving` frees every unmarked old-gen block. Most promoted
 * objects are saved because the young phase rewrites surviving young objects'
 * fields to the new addresses and `mark_young_to_old_refs` marks them from
 * there. What that misses is an object whose ONLY reference is a root slot or a
 * native side table — and CratonVM keeps `LinkedHashMap` / `LinkedHashSet` /
 * `LinkedList` / `TreeMap` / `TreeSet` state in exactly such side tables, so
 * they are the natural way to reach the gap from Java.
 *
 * The shape that matters is a collection that survives long enough to be
 * PROMOTED, in a cycle that also runs the old sweep. Hence: keep every bundle
 * alive across many collections rather than dropping them, and re-verify the
 * OLD ones each round — a fresh collection that was never promoted cannot
 * express the bug. `System.gc()` is what makes the old sweep run below the 75%
 * occupancy threshold.
 *
 * `mp` adds the `hm_int_fast` overlay shape — a `Map.of(Character, ...)`, the
 * exact construction `org.springframework.http.server.DefaultPathContainer`
 * uses for its `SEPARATORS` table
 * (docs/known-issues/springboot/webflux-defaultpathcontainer-defaultseparator-classcast.md).
 * `Character` unboxes to `Value::Int`, which routes the backing map through
 * `hm_int_fast` instead of the LHM/TM side tables the other fields exercise —
 * a distinct overlay implementation, so it is not proven by the same code path
 * as `tm`/`ts`/`lhm`. Both `explicitGc` and allocation-pressure modes (see
 * below) cover it; the allocation-pressure mode is the one that matches real
 * Spring Boot suite runs, which never call `System.gc()` themselves and hit
 * this shape purely through natural promotion.
 *
 * Deterministic output; the runner diffs it against HotSpot.
 *
 *   cratonvm --java-home <jdk> -cp build ROverlaySystemGcStress
 *
 * Fails deterministically on an unfixed tree with JIT ON against a real JDK —
 * `AssertionError: tm size 0 != 24 (bundle 0)`, and on dev's tip before the fix,
 * `ClassCastException: class java.lang.Object cannot be cast to Bundle` (the
 * freed block after another allocation got it). `CRATONVM_OLD_SWEEP_JIT=0` also
 * makes it pass, which is what localised the defect to that sweep. `--nojit`
 * passes either way, so run it with the JIT ON.
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

    /** Reference type held only in the `hm_int_fast` overlay for `mp`, below —
     * mirrors DefaultPathContainer's `DefaultSeparator`: a small object whose
     * only path to the heap is the Character-keyed Map.of entry. */
    static final class MpVal {
        final char c;
        final String tag;

        MpVal(char c, String tag) {
            this.c = c;
            this.tag = tag;
        }
    }

    /** One generation's worth of overlay-backed collections, all cross-checked. */
    static final class Bundle {
        final int id;
        final LinkedHashMap<String, String> lhm = new LinkedHashMap<>();
        final LinkedHashSet<String> lhs = new LinkedHashSet<>();
        final LinkedList<String> ll = new LinkedList<>();
        final TreeMap<String, String> tm = new TreeMap<>();
        final TreeSet<String> ts = new TreeSet<>();
        // `hm_int_fast` overlay shape (Character key -> Value::Int unboxing),
        // same construction as DefaultPathContainer.SEPARATORS.
        final Map<Character, MpVal> mp;

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
            mp = Map.of(
                    '/', new MpVal('/', "%2F" + id),
                    '.', new MpVal('.', "%2E" + id));
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
            check(mp.size() == 2, "mp size " + mp.size() + " != 2 (bundle " + id + ")");
            MpVal slash = mp.get('/');
            MpVal dot = mp.get('.');
            check(slash != null && slash.c == '/' && ("%2F" + id).equals(slash.tag),
                    "mp lost or corrupted '/' (bundle " + id + "): " + (slash == null ? "null" : slash.tag));
            check(dot != null && dot.c == '.' && ("%2E" + id).equals(dot.tag),
                    "mp lost or corrupted '.' (bundle " + id + "): " + (dot == null ? "null" : dot.tag));
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
