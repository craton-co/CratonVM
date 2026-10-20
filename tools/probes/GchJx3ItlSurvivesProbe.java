import java.util.ArrayList;
import java.util.List;

/**
 * gch h3/jx: the soundness half of {@code CRATONVM_TL_INHERITED_PENDING_EPHEMERON}
 * (docs/internal/gc/gch-h2jr-compat-unstarted-thread-inherited-values-outlive-their-thread-by-collections-FIXED-20261012.md).
 *
 * Under the switch a never-started {@code Thread}'s inherited values are held by
 * the VM's pending table and kept alive only through their child {@code Thread}
 * (an ephemeron edge), no longer as strong roots. This probe checks the other
 * direction from {@code UnstartedThreadItlProbe}: a child that is still
 * reachable keeps its inherited value intact through young and full collections
 * (moving and in-place), while its unreachable siblings are dropped, and sees
 * exactly that value when it finally starts.
 *
 * Each child's value is a fresh {@code int[]} filled with the child's index,
 * built by {@code childValue} in the parent, plus a String. Half the children
 * are kept, half dropped. Garbage churn and {@code System.gc()} run before any
 * child starts. Stdout is deterministic; diagnostics go to stderr.
 */
public class GchJx3ItlSurvivesProbe {
    static final int THREADS = 400;
    static final int LEN = 256;
    static int next;

    static final InheritableThreadLocal<int[]> ARR = new InheritableThreadLocal<>() {
        @Override
        protected int[] childValue(int[] parent) {
            int[] fresh = new int[LEN];
            java.util.Arrays.fill(fresh, next);
            return fresh;
        }
    };

    static final InheritableThreadLocal<String> STR = new InheritableThreadLocal<>() {
        @Override
        protected String childValue(String parent) {
            return parent + "-" + next;
        }
    };

    static volatile Object sink;

    static void churn(int rounds) {
        for (int r = 0; r < rounds; r++) {
            Object[] keep = new Object[64];
            for (int i = 0; i < 20_000; i++) {
                byte[] b = new byte[64 + (i & 255)];
                keep[i & 63] = b;
            }
            sink = keep;
        }
    }

    public static void main(String[] args) throws Exception {
        ARR.set(new int[] {-1});
        STR.set("itl");
        List<Thread> kept = new ArrayList<>();
        int[] want = new int[THREADS];
        int[] got = new int[THREADS];
        String[] gotStr = new String[THREADS];
        for (int i = 0; i < THREADS; i++) {
            next = i;
            final int id = i;
            Thread t = new Thread(() -> {
                int[] v = ARR.get();
                int seen = -2;
                if (v != null && v.length == LEN) {
                    seen = v[0];
                    for (int x : v) {
                        if (x != seen) {
                            seen = -3;
                            break;
                        }
                    }
                }
                got[id] = seen;
                gotStr[id] = STR.get();
            });
            want[i] = i;
            if ((i & 1) == 0) {
                kept.add(t);
            }
            // odd children: dropped unstarted
        }
        long t0 = System.nanoTime();
        for (int round = 0; round < 4; round++) {
            churn(20);
            System.gc();
        }
        churn(40);
        System.err.println("[itl-survives] churn+gc ms=" + (System.nanoTime() - t0) / 1_000_000);
        for (Thread t : kept) {
            t.start();
        }
        for (Thread t : kept) {
            t.join();
        }
        int bad = 0;
        int badStr = 0;
        for (int i = 0; i < THREADS; i += 2) {
            if (got[i] != want[i]) {
                bad++;
                if (bad <= 5) {
                    System.err.println("[itl-survives] child " + i + " saw " + got[i]);
                }
            }
            if (!("itl-" + i).equals(gotStr[i])) {
                badStr++;
                if (badStr <= 5) {
                    System.err.println("[itl-survives] child " + i + " saw string " + gotStr[i]);
                }
            }
        }
        System.out.println("itl-survives: kept=" + kept.size() + " bad=" + bad + " badStr=" + badStr);
        System.out.println(bad == 0 && badStr == 0 ? "PROBE-OK" : "PROBE-FAIL");
    }
}
