import java.util.*;

/**
 * Drives virtual call sites at JDK methods hard enough to tier up and
 * populate inline caches, so `--jdk-only-report`'s
 * `jit_inline_cache_natives` counter has something to count.
 *
 * Item 11 §1 says strict mode "pays for the gap with a blanket refusal that
 * costs `JdkOnly` runs every inline-cached native call". That is a claim
 * about a rate, and the counter for it already exists; this is the workload
 * that makes it non-zero if it is true.
 *
 * Monomorphic and polymorphic sites separately: the MIC and the PIC are two
 * different slots with the same missing-kind shape, and a refusal at one
 * says nothing about the other.
 */
public class JdkOnlyIcHotProbe {
    static final int N = 400_000;

    public static void main(String[] args) {
        long t0 = System.nanoTime();
        System.out.println("mono   " + mono());
        System.out.println("poly   " + poly());
        System.out.println("iface  " + iface());
        System.out.println("string " + string());
        System.out.println("ICHOT done ms=" + (System.nanoTime() - t0) / 1_000_000);
    }

    /** One receiver class per site — populates the MIC. */
    static long mono() {
        HashMap<Integer, Integer> m = new HashMap<>();
        long acc = 0;
        for (int i = 0; i < N; i++) {
            m.put(i & 1023, i);
            acc += m.get(i & 1023);
            acc += m.size();
        }
        return acc;
    }

    /** Three receiver classes per site — populates the PIC. */
    static long poly() {
        List<List<Integer>> ls = List.of(
                new ArrayList<>(), new LinkedList<>(), new Vector<>());
        long acc = 0;
        for (int i = 0; i < N; i++) {
            List<Integer> l = ls.get(i % 3);
            l.add(i);
            acc += l.size();
            if (l.size() > 64) l.clear();
        }
        return acc;
    }

    /** Interface dispatch over four implementors — megamorphic. */
    static long iface() {
        Map<?, ?>[] ms = {
                new HashMap<>(), new TreeMap<>(), new LinkedHashMap<>(),
                new IdentityHashMap<>(), new WeakHashMap<>(), new Hashtable<>(),
        };
        long acc = 0;
        for (int i = 0; i < N; i++) acc += ms[i % ms.length].size() + 1;
        return acc;
    }

    /** String natives, which is where the forced-native policy lives. */
    static long string() {
        long acc = 0;
        String s = "the quick brown fox jumps over the lazy dog";
        for (int i = 0; i < N; i++) {
            acc += s.length() + s.indexOf('q') + s.charAt(i % 40);
            if ((i & 8191) == 0) acc += s.toUpperCase().hashCode();
        }
        return acc;
    }
}
