import java.lang.ref.WeakReference;
import java.util.concurrent.CountDownLatch;

/**
 * gci i25/st2 (2026-10-15): the String table's insert funnel and young-entry list
 * ({@code CRATONVM_GC_STRING_TABLE_FUNNEL}) against HotSpot 25
 * (gcg-g2w-proposal-string-table-keyed-by-the-string, steps 1-2).
 *
 * <p>Modes (first argument):
 * <ul>
 *   <li>{@code young [pool] [rounds]} -- interns {@code pool} (default 100,000) distinct Strings and
 *       keeps them (they tenure: the large, old pool of a literal-heavy application), then runs
 *       {@code rounds} (default 200) rounds of young-generation churn (16 MiB of short-lived
 *       {@code byte[1024]} each); every round also interns 1,000 transient Strings (dropped at
 *       once) and 100 kept for the round. Every kept String must stay the pooled one. The cost
 *       metric is the young-pause total ({@code -Xlog:gc:stderr}).</li>
 *   <li>{@code contend [threads] [n]} -- {@code threads} (default 4) threads start together and
 *       each interns {@code n} (default 200,000, at least 2,048) Strings: a shared set of 2,048
 *       texts (the first misses race between threads) and one private text per step (dropped).
 *       Every thread must answer the same object for each shared text, every time.</li>
 *   <li>{@code identity} -- young identity across young collections: a young interned String the
 *       young pauses move stays the pooled one; a dropped interned String is re-pooled as a new
 *       receiver after a full collection; half of 10,000 interns kept across young pauses keep
 *       their identity.</li>
 * </ul>
 *
 * <p>Stdout is deterministic and identical on HotSpot 25. Stderr: {@code elapsed-ms=}.
 */
public class GciSt25InternPoolProbe {
    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "identity";
        long t0 = System.nanoTime();
        switch (mode) {
            case "young" -> young(intArg(args, 1, 100_000), intArg(args, 2, 200));
            case "contend" -> contend(intArg(args, 1, 4), intArg(args, 2, 200_000));
            case "identity" -> identity();
            default -> throw new IllegalArgumentException(mode);
        }
        System.err.println("elapsed-ms=" + (System.nanoTime() - t0) / 1_000_000);
        System.out.println("PROBE-OK " + mode);
    }

    static int intArg(String[] args, int i, int dflt) {
        return args.length > i ? Integer.parseInt(args[i]) : dflt;
    }

    static String built(String prefix, int i) {
        return new StringBuilder(prefix).append(i).toString();
    }

    static void churn(int kib) {
        for (int i = 0; i < kib; i++) {
            sink = new byte[1024];
        }
    }

    static void young(int poolSize, int rounds) {
        String[] pool = new String[poolSize];
        for (int i = 0; i < poolSize; i++) {
            pool[i] = built("st2-pool-", i).intern();
        }
        String[] roundKept = new String[100];
        int roundMismatches = 0;
        for (int r = 0; r < rounds; r++) {
            String transientPrefix = "st2-t" + r + "-";
            String keptPrefix = "st2-k" + r + "-";
            for (int j = 0; j < 1000; j++) {
                sink = built(transientPrefix, j).intern();
            }
            for (int j = 0; j < 100; j++) {
                roundKept[j] = built(keptPrefix, j).intern();
            }
            churn(16 * 1024);
            for (int j = 0; j < 100; j++) {
                if (roundKept[j] != built(keptPrefix, j).intern()) {
                    roundMismatches++;
                }
            }
        }
        sink = null;
        int same = 0;
        for (int i = 0; i < poolSize; i++) {
            if (pool[i] == built("st2-pool-", i).intern()) {
                same++;
            }
        }
        System.out.println("young pool=" + poolSize + " rounds=" + rounds
                + " pool-identical=" + same + "/" + poolSize
                + " round-mismatches=" + roundMismatches);
    }

    static void contend(int threads, int n) throws Exception {
        final int shared = 2048;
        if (n < shared) {
            throw new IllegalArgumentException("n must be at least " + shared);
        }
        final String[][] seen = new String[threads][shared];
        final CountDownLatch go = new CountDownLatch(1);
        Thread[] workers = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int id = t;
            workers[t] = new Thread(() -> {
                try {
                    go.await();
                } catch (InterruptedException e) {
                    throw new RuntimeException(e);
                }
                String privatePrefix = "st2-p" + id + "-";
                for (int i = 0; i < n; i++) {
                    int k = i % shared;
                    String s = built("st2-shared-", k).intern();
                    if (i < shared) {
                        seen[id][k] = s;
                    } else if (seen[id][k] != s) {
                        seen[id][k] = null; // a changed answer stays a mismatch
                    }
                    sink = built(privatePrefix, i).intern();
                }
            });
            workers[t].start();
        }
        go.countDown();
        for (Thread w : workers) {
            w.join();
        }
        int mismatches = 0;
        for (int k = 0; k < shared; k++) {
            for (int t = 0; t < threads; t++) {
                if (seen[t][k] == null || seen[t][k] != seen[0][k]) {
                    mismatches++;
                }
            }
        }
        System.out.println("contend threads=" + threads + " n=" + n
                + " shared-mismatches=" + mismatches);
    }

    static WeakReference<String> dropOne() {
        String d = built("st2-id-", 2).intern();
        return new WeakReference<>(d);
    }

    static void identity() {
        String a = built("st2-id-", 1);
        boolean receiver = a.intern() == a;
        churn(64 * 1024);
        String again = built("st2-id-", 1).intern();
        System.out.println("young-receiver=" + receiver);
        System.out.println("moved-still-pooled=" + (again == a));

        WeakReference<String> w = dropOne();
        System.gc();
        System.out.println("dropped-cleared=" + (w.get() == null));
        String b = built("st2-id-", 2);
        System.out.println("dropped-repooled-receiver=" + (b.intern() == b));

        String[] kept = new String[5000];
        for (int i = 0; i < 10_000; i++) {
            String s = built("st2-id-m-", i).intern();
            if ((i & 1) == 0) {
                kept[i >> 1] = s;
            }
            if (i % 1000 == 0) {
                churn(4 * 1024);
            }
        }
        churn(32 * 1024);
        int same = 0;
        for (int i = 0; i < 10_000; i += 2) {
            if (kept[i >> 1] == built("st2-id-m-", i).intern()) {
                same++;
            }
        }
        System.out.println("kept-identical=" + same + "/5000");
    }
}
