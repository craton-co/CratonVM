import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ConcurrentMap;

/**
 * Regression: a map operation must give the SAME answer interpreted and after
 * tier-up, and both must equal HotSpot's.
 *
 * <h2>Why this vector exists</h2>
 *
 * {@code H4-1} O1 named the JIT's collection direct helpers' failure mode as
 * "a tier-dependent wrong answer no arm diffs for". {@code H7-1} then found
 * three real disagreements inside them, the sharpest being a {@code put} whose
 * out-of-contract arm re-dispatched the put AFTER the overlay insert had
 * already happened, so the second execution returned the value that same call
 * had just written as the "previous mapping".
 *
 * Every arm of this suite runs each vector ONCE. A method that is wrong only
 * after tier-up — or only before it — is invisible to all of them. That is the
 * gap this file closes, and it is {@code H7-1} N1.
 *
 * <h2>What binds, and where</h2>
 *
 * Four call shapes reach {@code jit/src/lib.rs}'s collection ladders:
 *
 * <ul>
 *   <li>{@code invokevirtual java/util/HashMap.get} and {@code .put} — the
 *       {@code invoke_kind == 0} arm. {@code put} is recognised ONLY here;
 *       a {@code Map}-declared receiver's {@code put} is not recognised at
 *       all, which is why the {@code hNN} shapes below declare
 *       {@code HashMap} and the {@code mNN} shapes declare {@code Map}.</li>
 *   <li>{@code invokeinterface java/util/Map.get} — the {@code invoke_kind == 2}
 *       arm, whose helper runs {@code native_hashmap_get_exact}.</li>
 *   <li>{@code invokeinterface java/util/concurrent/ConcurrentMap.get} — the
 *       shape that binds {@code jit_concurrent_hashmap_get_direct}. Before this
 *       file, NO vector in {@code regression-suite/src} declared a
 *       {@code ConcurrentMap}-typed variable at all
 *       ({@code grep -l 'ConcurrentMap<' src/*.java} was empty), so that helper
 *       was unexercised by the whole corpus in either mode.</li>
 * </ul>
 *
 * <h2>Why the calls sit in per-shape methods, not in a loop</h2>
 *
 * The three ladders are SINGLE-PASS-BACKEND ONLY. Only
 * {@code Thread.currentThread}, {@code Preconditions.checkIndex} and
 * {@code Reference.reachabilityFence} were added at the optimizing/OSR door, so
 * a method that reaches the optimizing tier — or that is OSR-compiled because a
 * hot loop lives inside it — never binds a collection helper. A loop full of
 * {@code HashMap.get} that only gets hot via OSR therefore exercises NOTHING
 * here.
 *
 * So each shape's map call sites live in their OWN small static method, driven
 * by an invocation-counted outer loop. That crosses the C1 invocation threshold
 * ({@code c1_threshold = 500}, {@code jit/src/tiered.rs}) through the ordinary
 * front door. {@code ITERS = 3000} leaves margin: crossing the threshold only
 * ENQUEUES the method and the compiled entry is installed some time later.
 * Every loop here is also kept well under {@code osr_threshold = 10000}
 * back-edges on purpose, so the driver loop itself does not OSR-compile and
 * turn the driver into the thing under test.
 *
 * <h2>How a failure reads</h2>
 *
 * Each shape publishes {@code cold=[..] hot=[..] moved=N}. {@code cold} is the
 * first (interpreted) invocation, {@code hot} the last, {@code moved} the
 * iteration at which the answer first changed, or {@code -1}. A tier-dependent
 * wrong answer shows up three ways at once: the {@code CK} line differs from
 * HotSpot's, {@code moved} is non-negative, and the HOT (or COLD) assertion
 * against the oracle table fires. A vector that only read each shape once would
 * measure whichever tier happened to be right. Same design as
 * {@code RJitMultiArrayClass} and {@code RArrayStoreTiers}, for the same reason.
 *
 * <h2>Run it BOTH ways</h2>
 *
 * Normally and with {@code --nojit}. Red without the flag and green with it
 * isolates a divergence to the compiled tier. Under {@code --jdk-only} all four
 * triples are refused at BIND time by {@code jit::direct_native_helper} (all
 * four are {@code bridge} in
 * {@code scripts/baselines/jdk-only-kind-map-25-linux.tsv}), so strict mode has
 * no second tier for these calls and this vector PINS that: every
 * {@code moved} must still read {@code -1}, and every answer must still equal
 * the oracle.
 */
public final class RJitMapTierDiff {

    private static final int ITERS = 3000;
    /** The two fill-and-iterate shapes are ~200x the work of the rest; still >> 500. */
    private static final int FILL_ITERS = 700;
    private static final int FILL_N = 200;

    private static final List<String> DIVERGENCES = new ArrayList<>();

    private static void ck(String name, String evidence) {
        System.out.println("CK RJitMapTierDiff " + name + " " + evidence);
    }

    private static void diverge(String message) {
        DIVERGENCES.add(message);
        System.out.println("FAILED RJitMapTierDiff " + message);
    }

    private static String str(Object o) {
        return String.valueOf(o);
    }

    // --- keys the fast paths screen on ------------------------------------

    /** A key whose hashCode() ALLOCATES — the shape a GC can move under. */
    private static final class AllocKey {
        final int id;
        AllocKey(int id) { this.id = id; }
        @Override public int hashCode() { return ("alloc-" + id).hashCode(); }
        @Override public boolean equals(Object o) {
            return o instanceof AllocKey && ((AllocKey) o).id == id;
        }
    }

    /** Constant hashCode: every instance lands in one bucket, forcing the chain. */
    private static final class Collide {
        final int id;
        Collide(int id) { this.id = id; }
        @Override public int hashCode() { return 42; }
        @Override public boolean equals(Object o) {
            return o instanceof Collide && ((Collide) o).id == id;
        }
    }

    // --- HashMap, invokevirtual java/util/HashMap -------------------------
    //
    // One method per shape so each has its own invocation counter and its own
    // compiled artifact, and so `cold` is a genuinely interpreted reading for
    // every shape rather than only for the first one scheduled.

    /** put() into an absent key returns null. */
    private static String h00() {
        HashMap<Object, Object> m = new HashMap<>();
        return str(m.put(Integer.valueOf(7), "a"));
    }

    /** put() over an existing key returns the PREVIOUS mapping. H7-1 2c. */
    private static String h01() {
        HashMap<Object, Object> m = new HashMap<>();
        m.put(Integer.valueOf(7), "a");
        return str(m.put(Integer.valueOf(7), "b"));
    }

    /** …and the map then holds the NEW value. */
    private static String h02() {
        HashMap<Object, Object> m = new HashMap<>();
        m.put(Integer.valueOf(7), "a");
        m.put(Integer.valueOf(7), "b");
        return str(m.get(Integer.valueOf(7)));
    }

    private static String h03() {
        HashMap<Object, Object> m = new HashMap<>();
        m.put(Integer.valueOf(7), "a");
        return str(m.get(Integer.valueOf(9)));
    }

    /** A null key is legal in HashMap and must survive the round trip. */
    private static String h04() {
        HashMap<Object, Object> m = new HashMap<>();
        m.put(null, "n");
        return str(m.get(null));
    }

    private static String h05() {
        HashMap<Object, Object> m = new HashMap<>();
        m.put(null, "n1");
        return str(m.put(null, "n2"));
    }

    private static String h06() {
        HashMap<Object, Object> m = new HashMap<>();
        m.put("sk", "sv");
        return str(m.get("sk"));
    }

    /** Lookup by an EQUAL-but-not-identical key whose hashCode allocates. */
    private static String h07() {
        HashMap<Object, Object> m = new HashMap<>();
        m.put(new AllocKey(3), "av");
        return str(m.get(new AllocKey(3)));
    }

    /** Three keys, one bucket: the chain walk, not the first slot. */
    private static String h08() {
        HashMap<Object, Object> m = new HashMap<>();
        m.put(new Collide(0), "c0");
        m.put(new Collide(1), "c1");
        m.put(new Collide(2), "c2");
        return str(m.get(new Collide(0))) + "|"
             + str(m.get(new Collide(1))) + "|"
             + str(m.get(new Collide(2)));
    }

    /** Integer OUTSIDE the valueOf cache: two distinct objects, equal by equals(). */
    private static String h09() {
        HashMap<Object, Object> m = new HashMap<>();
        m.put(Integer.valueOf(10_000), "big");
        return str(m.get(Integer.valueOf(10_000)));
    }

    /**
     * The sharpest H7-1 2c probe: five successive put()s of ONE key, collecting
     * every returned previous mapping. An arm that inserts and then re-dispatches
     * the same put reports the value it just wrote, so this reads
     * "null|v1|v2|v3|v4" instead of "null|v0|v1|v2|v3".
     */
    private static String h10() {
        HashMap<Object, Object> m = new HashMap<>();
        StringBuilder sb = new StringBuilder();
        for (int k = 0; k < 5; k++) {
            if (k > 0) sb.append('|');
            sb.append(str(m.put(Integer.valueOf(4), "v" + k)));
        }
        return sb.toString();
    }

    /**
     * The "silently empty map" shape (H4-1 1, H0-4 4): inserts that go to a VM
     * side structure while size() and the iterator read the real table report a
     * container with fewer entries than were put into it.
     */
    private static String h11() {
        HashMap<Object, Object> m = new HashMap<>();
        for (int k = 0; k < FILL_N; k++) m.put(Integer.valueOf(k), Integer.valueOf(k));
        int iter = 0;
        long sum = 0;
        for (Map.Entry<Object, Object> e : m.entrySet()) {
            iter++;
            sum += ((Integer) e.getValue()).intValue();
        }
        return "size=" + m.size() + " iter=" + iter + " sum=" + sum;
    }

    /** remove() also returns the previous mapping, and empties the slot. */
    private static String h12() {
        HashMap<Object, Object> m = new HashMap<>();
        m.put(Integer.valueOf(7), "a");
        m.put(Integer.valueOf(7), "b");
        return str(m.remove(Integer.valueOf(7))) + "|" + str(m.get(Integer.valueOf(7)));
    }

    // --- Map, invokeinterface java/util/Map.get ---------------------------
    //
    // The receiver is declared Map, so javac emits the INTERFACE owner. That is
    // the triple the invoke_kind == 2 arm asks its policy question about, while
    // the helper it binds runs native_hashmap_get_exact — H7-1 4a.

    private static String m00() {
        HashMap<Object, Object> hm = new HashMap<>();
        hm.put(Integer.valueOf(7), "b");
        Map<Object, Object> m = hm;
        return str(m.get(Integer.valueOf(7)));
    }

    private static String m01() {
        HashMap<Object, Object> hm = new HashMap<>();
        hm.put(Integer.valueOf(7), "b");
        Map<Object, Object> m = hm;
        return str(m.get(Integer.valueOf(9)));
    }

    private static String m02() {
        HashMap<Object, Object> hm = new HashMap<>();
        hm.put(null, "n");
        Map<Object, Object> m = hm;
        return str(m.get(null));
    }

    private static String m03() {
        HashMap<Object, Object> hm = new HashMap<>();
        hm.put(new AllocKey(3), "av");
        Map<Object, Object> m = hm;
        return str(m.get(new AllocKey(3)));
    }

    private static String m04() {
        HashMap<Object, Object> hm = new HashMap<>();
        hm.put(new Collide(0), "c0");
        hm.put(new Collide(1), "c1");
        Map<Object, Object> m = hm;
        return str(m.get(new Collide(1)));
    }

    // --- ConcurrentMap, invokeinterface java/util/concurrent/ConcurrentMap.get
    //
    // The shape the corpus had NONE of. jit_concurrent_hashmap_get_direct binds
    // only here: a ConcurrentHashMap-declared receiver emits
    // `invokevirtual ConcurrentHashMap.get` and misses the ladder entirely,
    // which is why RChmKeySetView — every receiver of which is declared
    // ConcurrentHashMap or Map — never reached it.

    private static String c00() {
        ConcurrentHashMap<Object, Object> chm = new ConcurrentHashMap<>();
        chm.put(Integer.valueOf(7), "b");
        ConcurrentMap<Object, Object> cm = chm;
        return str(cm.get(Integer.valueOf(7)));
    }

    private static String c01() {
        ConcurrentHashMap<Object, Object> chm = new ConcurrentHashMap<>();
        chm.put(Integer.valueOf(7), "b");
        ConcurrentMap<Object, Object> cm = chm;
        return str(cm.get(Integer.valueOf(9)));
    }

    /**
     * ConcurrentHashMap forbids a null key. An unvalidated key that answers
     * `null` instead of taking the contract path is H7-1 2a's exact shape, and
     * a helper that swallows it reads "null" here where HotSpot reads "NPE".
     */
    private static String c02() {
        ConcurrentHashMap<Object, Object> chm = new ConcurrentHashMap<>();
        chm.put(Integer.valueOf(7), "b");
        ConcurrentMap<Object, Object> cm = chm;
        try {
            return str(cm.get(null));
        } catch (NullPointerException e) {
            return "NPE";
        }
    }

    private static String c03() {
        ConcurrentHashMap<Object, Object> chm = new ConcurrentHashMap<>();
        chm.put("sk", "sv");
        ConcurrentMap<Object, Object> cm = chm;
        return str(cm.get("sk"));
    }

    private static String c04() {
        ConcurrentHashMap<Object, Object> chm = new ConcurrentHashMap<>();
        chm.put(new AllocKey(3), "av");
        ConcurrentMap<Object, Object> cm = chm;
        return str(cm.get(new AllocKey(3)));
    }

    private static String c05() {
        ConcurrentHashMap<Object, Object> chm = new ConcurrentHashMap<>();
        ConcurrentMap<Object, Object> cm = chm;
        cm.put(Integer.valueOf(7), "a");
        return str(cm.put(Integer.valueOf(7), "b"));
    }

    private static String c06() {
        ConcurrentHashMap<Object, Object> chm = new ConcurrentHashMap<>();
        ConcurrentMap<Object, Object> cm = chm;
        for (int k = 0; k < FILL_N; k++) cm.put(Integer.valueOf(k), Integer.valueOf(k));
        int iter = 0;
        long sum = 0;
        for (Map.Entry<Object, Object> e : cm.entrySet()) {
            iter++;
            sum += ((Integer) e.getValue()).intValue();
        }
        return "size=" + cm.size() + " iter=" + iter + " sum=" + sum;
    }

    // --- vector table -----------------------------------------------------

    private static final String[] NAMES = {
        "h00-put-insert-returns-null",
        "h01-put-returns-previous",
        "h02-get-after-overwrite",
        "h03-get-absent",
        "h04-null-key-roundtrip",
        "h05-null-key-put-returns-previous",
        "h06-string-key",
        "h07-alloc-hash-key",
        "h08-collide-chain",
        "h09-boxed-outside-cache",
        "h10-put-sequence-previous-values",
        "h11-fill-size-iterate",
        "h12-remove-returns-previous",
        "m00-iface-get-present",
        "m01-iface-get-absent",
        "m02-iface-get-null-key",
        "m03-iface-get-alloc-hash",
        "m04-iface-get-collide",
        "c00-cmap-get-present",
        "c01-cmap-get-absent",
        "c02-cmap-get-null-key",
        "c03-cmap-get-string",
        "c04-cmap-get-alloc-hash",
        "c05-cmap-put-returns-previous",
        "c06-cmap-fill-size-iterate",
    };

    /**
     * MEASURED on HotSpot 25.0.3+9 (see the record for the oracle column), not
     * assumed. run.sh diffs stdout against the same oracle on every run, so a
     * wrong entry here fails on BOTH VMs rather than silently passing.
     */
    private static final String[] EXPECTED = {
        "null",
        "a",
        "b",
        "null",
        "n",
        "n1",
        "sv",
        "av",
        "c0|c1|c2",
        "big",
        "null|v0|v1|v2|v3",
        "size=200 iter=200 sum=19900",
        "b|null",
        "b",
        "null",
        "n",
        "av",
        "c1",
        "b",
        "null",
        "NPE",
        "sv",
        "av",
        "a",
        "size=200 iter=200 sum=19900",
    };

    private static String observe(int id) {
        switch (id) {
            case 0:  return h00();
            case 1:  return h01();
            case 2:  return h02();
            case 3:  return h03();
            case 4:  return h04();
            case 5:  return h05();
            case 6:  return h06();
            case 7:  return h07();
            case 8:  return h08();
            case 9:  return h09();
            case 10: return h10();
            case 11: return h11();
            case 12: return h12();
            case 13: return m00();
            case 14: return m01();
            case 15: return m02();
            case 16: return m03();
            case 17: return m04();
            case 18: return c00();
            case 19: return c01();
            case 20: return c02();
            case 21: return c03();
            case 22: return c04();
            case 23: return c05();
            case 24: return c06();
            default: throw new AssertionError("no such shape " + id);
        }
    }

    private static boolean isFillShape(int id) {
        return id == 11 || id == 24;
    }

    public static void main(String[] args) {
        int checks = 0;

        for (int id = 0; id < NAMES.length; id++) {
            int iters = isFillShape(id) ? FILL_ITERS : ITERS;

            String cold = null;
            String hot = null;
            String moved = null;
            int movedAt = -1;

            for (int i = 0; i < iters; i++) {
                String a = observe(id);
                if (i == 0) {
                    cold = a;
                } else if (movedAt < 0 && !a.equals(cold)) {
                    movedAt = i;
                    moved = a;
                }
                hot = a;
            }

            String name = NAMES[id];
            String want = EXPECTED[id];

            // Evidence FIRST, verdicts after, so a FAILED line always follows
            // the values it is about. `moved` is published on every row and not
            // only on a transition: the iteration at which an answer moved is
            // precisely the shape a tier-dependent wrong answer has.
            ck(name, "cold=[" + cold + "] hot=[" + hot + "] moved=" + movedAt + " iters=" + iters);

            checks++;
            if (!want.equals(cold)) {
                diverge(name + " COLD: want=[" + want + "] got=[" + cold + "]");
            }
            checks++;
            if (!want.equals(hot)) {
                diverge(name + " HOT: want=[" + want + "] got=[" + hot + "]");
            }
            checks++;
            if (movedAt >= 0) {
                diverge(name + " TIER-SPLIT at i=" + movedAt
                    + ": cold=[" + cold + "] became=[" + moved + "] final=[" + hot + "]");
            }
        }

        // SEPARATE lines, and in this order — harness_check_count does
        // `sub(/^.*checks=/, ""); print`, so `checks=75 fails=0` would publish
        // the "count" `75 fails=0`. Same note as RJitMultiArrayClass.
        System.out.println("CK RJitMapTierDiff fails=" + DIVERGENCES.size());
        System.out.println("CK RJitMapTierDiff checks=" + checks);
        if (!DIVERGENCES.isEmpty()) {
            throw new AssertionError(DIVERGENCES.size() + " divergence(s)");
        }
        System.out.println("PASS RJitMapTierDiff (" + checks + " checks)");
    }
}
