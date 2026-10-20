import java.io.InputStream;
import java.lang.ref.WeakReference;

/**
 * gcg g2/w (2026-10-01): the interned-String table against HotSpot
 * (common-w9c-proposal-weak-string-intern-table).
 *
 * <p>Modes (first argument):
 * <ul>
 *   <li>{@code identity} -- {@code String.intern()}'s miss rule: a fresh String
 *       is pooled ITSELF ({@code s.intern() == s}), a second equal String gets
 *       the first, a literal resolved first wins, and a literal resolved AFTER
 *       an equal String was interned resolves to that String.</li>
 *   <li>{@code reclaim [n]} -- interns {@code n} (default 2,000,000) distinct
 *       computed Strings and drops them, keeping every 100,000th; three
 *       {@code System.gc()} on the way. The table must not hold the dropped
 *       ones (a strong table runs a 64 MiB heap out of memory) and must keep
 *       the identity of the kept ones across the full collections.</li>
 *   <li>{@code literal} -- a literal its class still uses stays alive: a
 *       {@code WeakReference} to it never clears, and the next {@code ldc}
 *       answers the same object.</li>
 *   <li>{@code unload} -- a literal of a class whose loader died is reclaimed:
 *       a {@code WeakReference} to it clears (HotSpot, Serial: class unloading
 *       at the full collection, then the StringTable clean).</li>
 * </ul>
 *
 * <p>Stdout is deterministic and identical on HotSpot 25. Stderr: elapsed ms.
 */
public class GcgW2InternProbe {
    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "identity";
        long t0 = System.nanoTime();
        switch (mode) {
            case "identity" -> identity();
            case "reclaim" -> reclaim(args.length > 1 ? Integer.parseInt(args[1]) : 2_000_000);
            case "literal" -> literal();
            case "unload" -> unload();
            default -> throw new IllegalArgumentException(mode);
        }
        System.err.println("elapsed-ms=" + (System.nanoTime() - t0) / 1_000_000);
        System.out.println("PROBE-OK " + mode);
    }

    static String built(String a, String b) {
        return new StringBuilder(a).append(b).toString();
    }

    static void identity() {
        String a = built("gcg-w2-", "fresh");
        System.out.println("fresh-receiver=" + (a.intern() == a));
        String b = built("gcg-w2-", "fresh");
        System.out.println("second-gets-first=" + (b.intern() == a));
        System.out.println("second-not-receiver=" + (b.intern() != b));

        String lit = "gcg-w2-lit1";
        String c = built("gcg-w2-", "lit1");
        System.out.println("literal-first-wins=" + (c.intern() == lit));
        System.out.println("literal-first-not-receiver=" + (c.intern() != c));

        String d = built("gcg-w2-", "lit2");
        String di = d.intern();
        System.out.println("intern-first-receiver=" + (di == d));
        System.out.println("later-literal-is-the-interned=" + ("gcg-w2-lit2" == d));
    }

    static void reclaim(int n) {
        String[] kept = new String[(n + 99_999) / 100_000];
        int[] keptAt = new int[kept.length];
        int k = 0;
        for (int i = 0; i < n; i++) {
            String s = built("gcg-w2-r-", Integer.toString(i)).intern();
            sink = s;
            if (i % 100_000 == 0) {
                kept[k] = s;
                keptAt[k] = i;
                k++;
            }
            if (i > 0 && i % (n / 4 + 1) == 0) {
                System.gc();
            }
        }
        sink = null;
        int same = 0;
        for (int j = 0; j < k; j++) {
            if (kept[j] == built("gcg-w2-r-", Integer.toString(keptAt[j])).intern()) {
                same++;
            }
        }
        System.out.println("reclaim n=" + n + " kept-identical=" + same + "/" + k);
    }

    static String held() {
        return "gcg-w2-held-literal";
    }

    static void churn(int rounds) {
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < 2_000; i++) {
                sink = new byte[4096];
            }
            System.gc();
        }
        sink = null;
    }

    static void literal() {
        WeakReference<String> w = new WeakReference<>(held());
        churn(5);
        System.out.println("held-literal-alive=" + (w.get() != null));
        System.out.println("held-literal-identity=" + (w.get() == held()));
        String again = built("gcg-w2-held-", "literal").intern();
        System.out.println("held-literal-is-interned=" + (again == held()));
    }

    /** Loaded only through {@link OneShot}; its literal appears nowhere else. */
    public static final class Lit {
        public static String lit() {
            return "gcg-w2-unload-literal";
        }
    }

    static final class OneShot extends ClassLoader {
        OneShot() {
            super(null);
        }

        Class<?> define(byte[] b) {
            return defineClass("GcgW2InternProbe$Lit", b, 0, b.length);
        }
    }

    static WeakReference<?>[] loadAndTouch() throws Exception {
        byte[] bytes;
        try (InputStream in = GcgW2InternProbe.class.getResourceAsStream("GcgW2InternProbe$Lit.class")) {
            bytes = in.readAllBytes();
        }
        OneShot loader = new OneShot();
        Class<?> k = loader.define(bytes);
        String s = (String) k.getMethod("lit").invoke(null);
        System.out.println("unload-literal-text=" + s.length());
        return new WeakReference<?>[] {new WeakReference<>(s), new WeakReference<>(loader)};
    }

    static void unload() throws Exception {
        WeakReference<?>[] w = loadAndTouch();
        churn(10);
        System.out.println("loader-cleared=" + (w[1].get() == null));
        System.out.println("unloaded-literal-cleared=" + (w[0].get() == null));
    }
}
