import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

/**
 * Stresses the object a NATIVE returns to COMPILED code.
 *
 * <p>`try_jit_leaf_native_dispatch` in `vm/src/jit/helpers.rs` serves a native
 * straight from a compiled call site, skipping `invoke_or_native`. It hands the
 * caller a bare address. Every other object-returning JIT native fast path also
 * parks that address in `thread.native_pending_return` — the slot the collector
 * scans (`roots.rs`) and remaps (`gc.rs`) — but this path's general callback arm
 * does not; only its `Thread.currentThread()` arm does.
 *
 * <p>The question this probe exists to answer is whether that matters: is there
 * a window, between the native returning and the compiled caller storing the
 * value, in which a collection can move or reclaim the object? If there is, a
 * reference-returning method hands compiled code a dead reference.
 *
 * <p>Each loop below is shaped to open that window as wide as it can be opened
 * from Java: call a native that returns an object, allocate (so a young
 * collection can land), and only then read the returned object back. Run hot
 * enough to compile, and verify content AND identity where the JDK guarantees
 * it.
 *
 * <pre>
 * javac -d /tmp/probe probes/JitNativeReturnProbe.java
 * java  -cp /tmp/probe JitNativeReturnProbe                      # control
 * CRATONVM_DBG=gc-stress=65536 cratonvm --java-home &lt;jdk&gt; -cp /tmp/probe JitNativeReturnProbe
 * </pre>
 */
public class JitNativeReturnProbe {

    private static int failures = 0;
    /** Kept live so the allocations below actually survive and press the heap. */
    private static final List<Object> KEEP = new ArrayList<>();

    private static void check(String what, Object expected, Object actual) {
        boolean ok = expected == null ? actual == null : expected.equals(actual);
        if (!ok) {
            System.out.println("  FAIL " + what + " expected=" + expected + " actual=" + actual);
            failures++;
        }
    }

    private static void checkTrue(String what, boolean ok) {
        if (!ok) {
            System.out.println("  FAIL " + what);
            failures++;
        }
    }

    /** Allocation between the native's return and the use of its result. */
    private static void churn(int i) {
        byte[] b = new byte[128];
        b[0] = (byte) i;
        if ((i & 1023) == 0) {
            KEEP.add(b);
            if (KEEP.size() > 256) {
                KEEP.clear();
            }
        }
    }

    /** `StringBuilder.toString()` — a native returning a FRESH object. */
    private static void freshObjectReturn(int iters) {
        System.out.println("== StringBuilder.toString() across an allocation");
        int bad = 0;
        for (int i = 0; i < iters; i++) {
            StringBuilder sb = new StringBuilder();
            sb.append("abc").append(i & 7);
            String s = sb.toString();
            churn(i);
            if (s == null || s.length() != 4 || s.charAt(0) != 'a'
                    || s.charAt(3) != (char) ('0' + (i & 7))) {
                bad++;
            }
        }
        check("  corrupted returns", Integer.valueOf(0), Integer.valueOf(bad));
    }

    /** `Object.getClass()` — a native returning a long-lived, IDENTITY-stable object. */
    private static void identityStableReturn(int iters) {
        System.out.println("== getClass() identity across an allocation");
        Object o = new JitNativeReturnProbe();
        Class<?> expected = o.getClass();
        int bad = 0;
        for (int i = 0; i < iters; i++) {
            Class<?> c = o.getClass();
            churn(i);
            if (c != expected || !c.getName().equals("JitNativeReturnProbe")) {
                bad++;
            }
        }
        check("  identity breaks", Integer.valueOf(0), Integer.valueOf(bad));
    }

    /** `String.valueOf` / `String.substring` — natives returning derived objects. */
    private static void derivedStringReturns(int iters) {
        System.out.println("== String natives across an allocation");
        String src = "the quick brown fox";
        int bad = 0;
        for (int i = 0; i < iters; i++) {
            String sub = src.substring(4, 9);
            churn(i);
            if (!"quick".equals(sub)) {
                bad++;
            }
            String v = String.valueOf(i & 15);
            churn(i);
            if (!v.equals(Integer.toString(i & 15))) {
                bad++;
            }
        }
        check("  corrupted returns", Integer.valueOf(0), Integer.valueOf(bad));
    }

    /** `Map.get` — the shape Spring's annotation caches actually use. */
    private static void mapGetReturns(int iters) {
        System.out.println("== Map.get across an allocation");
        Map<String, String> m = new HashMap<>();
        for (int i = 0; i < 32; i++) {
            m.put("k" + i, "v" + i);
        }
        int bad = 0;
        for (int i = 0; i < iters; i++) {
            String key = "k" + (i & 31);
            String got = m.get(key);
            churn(i);
            if (got == null || !got.equals("v" + (i & 31))) {
                bad++;
            }
        }
        check("  corrupted returns", Integer.valueOf(0), Integer.valueOf(bad));
    }

    /**
     * A static factory that cannot return null, called through a layer — the
     * exact shape that failed in Spring (`MergedAnnotations.from(..)` arriving
     * as null through `AnnotatedElementUtils.getAnnotations`).
     */
    private static Object factory(int i) {
        return java.util.Objects.requireNonNull(Integer.valueOf(i & 63));
    }

    private static Object indirect(int i) {
        return factory(i);
    }

    private static void neverNullFactory(int iters) {
        System.out.println("== a never-null factory through one call layer");
        int nulls = 0;
        for (int i = 0; i < iters; i++) {
            Object o = indirect(i);
            churn(i);
            if (o == null) {
                nulls++;
            }
        }
        check("  nulls from a never-null factory", Integer.valueOf(0), Integer.valueOf(nulls));
    }

    /** `Thread.currentThread()` — the arm that DOES root, as a positive control. */
    private static void currentThreadReturn(int iters) {
        System.out.println("== Thread.currentThread() across an allocation (control)");
        Thread expected = Thread.currentThread();
        int bad = 0;
        for (int i = 0; i < iters; i++) {
            Thread t = Thread.currentThread();
            churn(i);
            if (t != expected) {
                bad++;
            }
        }
        check("  identity breaks", Integer.valueOf(0), Integer.valueOf(bad));
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 300_000;
        // Two passes: the first compiles the loops, the second runs them warm,
        // which is when the site cache is actually serving the calls.
        for (int pass = 0; pass < 2; pass++) {
            freshObjectReturn(iters);
            identityStableReturn(iters);
            derivedStringReturns(iters);
            mapGetReturns(iters);
            neverNullFactory(iters);
            currentThreadReturn(iters);
        }
        checkTrue("KEEP survived", KEEP != null);
        System.out.println(failures == 0 ? "PROBE PASS" : "PROBE FAIL (" + failures + ")");
        if (failures != 0) {
            System.exit(1);
        }
    }
}
