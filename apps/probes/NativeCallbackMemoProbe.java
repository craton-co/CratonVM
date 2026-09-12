import java.util.HashMap;
import java.util.Map;
import java.util.TreeMap;

/**
 * A native that calls Java by name, on a workload where that IS the hot path.
 *
 * `composition-native-callback-and-the-promotion-question-20260902.md` item 1
 * is about `NativeContext::invoke_virtual(receiver, "someMethod", "()V", &[])`
 * — how a registered Rust native calls back into Java — resolving its callee
 * BY NAME on every call. Its predecessor measured removing that at ONE call
 * site (`CompletableFuture.postComplete`) at 1.154x of a whole benchmark.
 *
 * `HibfixComposeProbe2` cannot price the GENERAL fix, and the reason is worth
 * stating: after the `postComplete` change it reaches the by-name resolver
 * through a native->Java callback only 0.035 times per chain
 * (`CRATONVM_DBG_CALLBACK_MEMO=1`: 1 415 probes in 40 000 chains). Its
 * remaining `invokes(general)` come from other doors entirely. A mechanism
 * measured on a workload that does not exercise it reads as "worth nothing"
 * for a reason that has nothing to do with the mechanism.
 *
 * This probe exercises it directly. Both loops drive a native collection stub
 * that must call a USER-DEFINED method on a key:
 *
 *  * `HashMap` — `native-collections`' map natives call
 *    `ctx.invoke_virtual(key, "hashCode", "()I", &[])` and
 *    `equals(Object)Z`, both ordinary bytecode on `Key`;
 *  * `TreeMap` — the sorted-map natives call
 *    `ctx.invoke_virtual(a, "compareTo", "(Ljava/lang/Object;)I", &[b])`,
 *    likewise bytecode on `Key`.
 *
 * Each callback is one full by-name resolution: the receiver's class name
 * `to_string()`ed under the class-manager read lock, the
 * `(class, method, descriptor)` triple hashed against ~3 100 native slots,
 * the descriptor-quirk rewrite on the miss, and then the whole resolution
 * again inside `invoke_on_class_shared_inner`.
 *
 * Prints ns per operation for each loop, interleaved and repeated, and a
 * median — the arms differ by less than the host drifts over one run, so a
 * single A-then-B pair is not a measurement (see
 * `JavaUtilTierUpExclusionProbe`'s rebuild note for the same trap).
 *
 * Arms: `CRATONVM_NATIVE_CALLBACK_MEMO=0` against the default, one binary.
 * Engagement: `CRATONVM_DBG_CALLBACK_MEMO=1` must show `hits` tracking
 * `probes` — a timing from a run whose memo never engaged says nothing.
 */
public class NativeCallbackMemoProbe {

    /** Ordinary bytecode `hashCode`/`equals`/`compareTo` — the callee. */
    static final class Key implements Comparable<Key> {
        final int v;

        Key(int v) {
            this.v = v;
        }

        @Override
        public int hashCode() {
            return v * 31 + 7;
        }

        @Override
        public boolean equals(Object o) {
            return o instanceof Key && ((Key) o).v == v;
        }

        @Override
        public int compareTo(Key o) {
            return Integer.compare(v, o.v);
        }

        @Override
        public String toString() {
            return "K" + v;
        }
    }

    private static final int KEYS = Integer.getInteger("probe.keys", 64);
    private static final int ROUNDS = Integer.getInteger("probe.rounds", 200_000);
    private static final int REPS = Integer.getInteger("probe.reps", 6);
    private static final int WARMUP = Integer.getInteger("probe.warmup", 20_000);

    static int sink;

    private static long hashLoop(Map<Key, Integer> m, Key[] keys, int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            Key k = keys[i % keys.length];
            m.put(k, i);
            Integer got = m.get(k);
            sink += got;
        }
        return System.nanoTime() - t0;
    }

    private static long treeLoop(Map<Key, Integer> m, Key[] keys, int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            Key k = keys[i % keys.length];
            m.put(k, i);
            Integer got = m.get(k);
            sink += got;
        }
        return System.nanoTime() - t0;
    }

    private static double median(double[] xs) {
        double[] c = xs.clone();
        java.util.Arrays.sort(c);
        int n = c.length;
        return (n % 2 == 1) ? c[n / 2] : (c[n / 2 - 1] + c[n / 2]) / 2.0;
    }

    private static double min(double[] xs) {
        double m = Double.MAX_VALUE;
        for (double x : xs) m = Math.min(m, x);
        return m;
    }

    private static double max(double[] xs) {
        double m = -Double.MAX_VALUE;
        for (double x : xs) m = Math.max(m, x);
        return m;
    }

    public static void main(String[] args) {
        Key[] keys = new Key[KEYS];
        for (int i = 0; i < KEYS; i++) keys[i] = new Key(i);

        Map<Key, Integer> hm = new HashMap<>();
        Map<Key, Integer> tm = new TreeMap<>();

        hashLoop(hm, keys, WARMUP);
        treeLoop(tm, keys, WARMUP);
        hashLoop(hm, keys, WARMUP);
        treeLoop(tm, keys, WARMUP);

        double[] hashNs = new double[REPS];
        double[] treeNs = new double[REPS];
        for (int r = 0; r < REPS; r++) {
            long h, t;
            if ((r & 1) == 0) {
                h = hashLoop(hm, keys, ROUNDS);
                t = treeLoop(tm, keys, ROUNDS);
            } else {
                t = treeLoop(tm, keys, ROUNDS);
                h = hashLoop(hm, keys, ROUNDS);
            }
            hashNs[r] = h / (double) ROUNDS;
            treeNs[r] = t / (double) ROUNDS;
            System.out.printf("@@NCBMEMO rep=%d hashmap=%9.1f treemap=%9.1f%n", r, hashNs[r], treeNs[r]);
        }

        System.out.printf("@@NCBMEMO hashmap median %9.1f ns/op [%.1f..%.1f]%n",
                median(hashNs), min(hashNs), max(hashNs));
        System.out.printf("@@NCBMEMO treemap median %9.1f ns/op [%.1f..%.1f]%n",
                median(treeNs), min(treeNs), max(treeNs));
        System.out.printf("@@NCBMEMO total  median %9.1f ns/op  sink=%d size=%d/%d%n",
                median(hashNs) + median(treeNs), sink, hm.size(), tm.size());
        System.out.println(hm.size() == KEYS && tm.size() == KEYS
                ? "@@NCBMEMO CLEAN" : "@@NCBMEMO DEFECT");
    }
}
