import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

// Prices the `receiver_is_java_util` PROMOTION exclusion, which
// `CRATONVM_JIT_VIRTUAL_PROMOTE_JAVA_UTIL=1` lifts.
//
// The exclusion bars `execute_invokevirtual_cached` from entering a compiled
// body directly when the receiver's class is under `java/util/`. Its own
// note says the policy "has never been priced on its own", because the
// measurement behind it compared `ReentrantLock` against a user subclass —
// two receiver classes in two loop methods, so the comparison carried
// "different class, different call site, different inlining" along with the
// tier-up. This probe puts the SAME receiver in ONE binary on both sides of
// the switch.
//
// The arms are deliberately split by what the receiver IS, because that is
// the only thing the gate looks at:
//
//   jdkMap / jdkList     receiver class is `java/util/…` — the gated arm.
//   userMap / userList   a user subclass of the same class, with the same
//                        bodies reached by the same bytecodes. The gate asks
//                        the RECEIVER's class id, so these are NOT gated, and
//                        they are the internal control.
//   nocall               no invoke at all.
//
// An arm that moves while `userMap`/`userList`/`nocall` sit still is the
// gate. Read `CRATONVM_DBG_FIELD_SITE=1`'s `virtual-promote: plain=N` FIRST:
// if it is 0 with the switch on, no call site promoted and the clock is
// measuring something else.
//
//   cratonvm --java-home <JDK 25> -c <dir> JavaUtilPromote 300000 7
public class JavaUtilPromote {
    static final class MyMap extends HashMap<String, String> {}
    static final class MyList extends ArrayList<String> {}

    static final String[] KEYS = new String[16];
    static {
        for (int i = 0; i < KEYS.length; i++) KEYS[i] = "k" + i;
    }

    static Map<String, String> fill(Map<String, String> m) {
        for (String k : KEYS) m.put(k, k + "v");
        return m;
    }

    static List<String> fillList(List<String> l) {
        for (String k : KEYS) l.add(k);
        return l;
    }

    static int mapLoop(int n, Map<String, String> m) {
        int s = 0;
        for (int i = 0; i < n; i++) s += m.get(KEYS[i & 15]).length();
        return s;
    }

    static int listLoop(int n, List<String> l) {
        int s = 0;
        for (int i = 0; i < n; i++) s += l.get(i & 15).length();
        return s;
    }

    static int none(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) s += i;
        return s;
    }

    public static void main(String[] a) {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 300000;
        int rounds = a.length > 1 ? Integer.parseInt(a[1]) : 7;
        Map<String, String> jdkMap = fill(new HashMap<>());
        Map<String, String> userMap = fill(new MyMap());
        List<String> jdkList = fillList(new ArrayList<>());
        List<String> userList = fillList(new MyList());
        String[] nm = {"nocall", "jdkMap", "userMap", "jdkList", "userList"};
        double[] m = new double[nm.length];
        for (int i = 0; i < m.length; i++) m[i] = 1e18;
        int sum = 0;
        long t;
        for (int r = 0; r < rounds; r++) {
            boolean fwd = (r & 1) == 0;
            for (int k = 0; k < nm.length; k++) {
                int j = fwd ? k : nm.length - 1 - k;
                t = System.nanoTime();
                switch (j) {
                    case 0:  sum += none(n); break;
                    case 1:  sum += mapLoop(n, jdkMap); break;
                    case 2:  sum += mapLoop(n, userMap); break;
                    case 3:  sum += listLoop(n, jdkList); break;
                    default: sum += listLoop(n, userList); break;
                }
                double d = (System.nanoTime() - t) / (double) n;
                if (d < m[j]) m[j] = d;
            }
        }
        for (int i = 0; i < nm.length; i++)
            System.out.println(nm[i] + "\t" + m[i] + "\tdelta=" + (m[i] - m[0]));
        if (sum == 42) System.out.println("x");
    }
}
