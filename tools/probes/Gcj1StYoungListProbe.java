import java.lang.ref.WeakReference;

/**
 * gcj j1/st (2026-10-19): identity and liveness of interned Strings across the young pauses
 * that move them, the promotions that take them off the young-entry list, and the full
 * collections that walk the whole pool -- the transitions the String table's young-entry list
 * ({@code CRATONVM_GC_STRING_TABLE_FUNNEL}) must survive with every row naming its String's
 * current address (gcj j1/st redesign; {@code gci-i47w-oa47}). Also the
 * {@code gch-h2og} shape: a young interned String whose {@code value} array is old.
 *
 * <p>Modes (first argument):
 * <ul>
 *   <li>{@code exact [n] [rounds]} -- each round interns {@code n} (default 2,000) fresh texts
 *       and keeps every other one, churns 8 MiB of young garbage (the kept Strings move, then
 *       tenure after a few rounds), and every fourth round runs {@code System.gc()} after taking
 *       weak references to that round's dropped Strings. At the end every kept String must still
 *       be the pooled one (same object, same text and hash), every weak reference taken before a
 *       {@code System.gc()} must be cleared, and a dropped text must re-pool its new receiver.</li>
 *   <li>{@code shared [n]} -- {@code n} (default 500) Strings are built and tenured, then each is
 *       copied with {@code new String(original)} (the copy shares the old {@code value}) and the
 *       young copy is interned and held only by a holder array that is dropped before a
 *       {@code System.gc()}; afterwards every text interns to a String whose content is intact.</li>
 * </ul>
 *
 * <p>Stdout is deterministic and identical on HotSpot 25 ({@code -XX:+UseSerialGC}). Stderr:
 * {@code elapsed-ms=}.
 */
public class Gcj1StYoungListProbe {
    static volatile Object sink;

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "exact";
        long t0 = System.nanoTime();
        switch (mode) {
            case "exact" -> exact(intArg(args, 1, 2_000), intArg(args, 2, 24));
            case "shared" -> shared(intArg(args, 1, 500));
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

    /** Interns the dropped half of round {@code r} and answers weak references to them. */
    static WeakReference<?>[] internDropped(int r, int n) {
        WeakReference<?>[] refs = new WeakReference<?>[n / 2];
        for (int j = 1, k = 0; j < n; j += 2, k++) {
            refs[k] = new WeakReference<>(built("gcj1st-d" + r + "-", j).intern());
        }
        return refs;
    }

    static void exact(int n, int rounds) {
        String[][] kept = new String[rounds][n / 2];
        int weakTaken = 0;
        int weakCleared = 0;
        for (int r = 0; r < rounds; r++) {
            String prefix = "gcj1st-k" + r + "-";
            for (int j = 0, k = 0; j < n; j += 2, k++) {
                kept[r][k] = built(prefix, j).intern();
            }
            boolean full = r % 4 == 3;
            if (full) {
                WeakReference<?>[] refs = internDropped(r, n);
                churn(1024);
                System.gc();
                for (WeakReference<?> w : refs) {
                    weakTaken++;
                    if (w.get() == null) {
                        weakCleared++;
                    }
                }
            } else {
                for (int j = 1; j < n; j += 2) {
                    sink = built("gcj1st-d" + r + "-", j).intern();
                }
            }
            churn(8 * 1024);
        }
        sink = null;
        int same = 0;
        int content = 0;
        int total = 0;
        for (int r = 0; r < rounds; r++) {
            String prefix = "gcj1st-k" + r + "-";
            for (int j = 0, k = 0; j < n; j += 2, k++) {
                total++;
                String text = built(prefix, j);
                String pooled = text.intern();
                if (pooled == kept[r][k]) {
                    same++;
                }
                if (kept[r][k].equals(text) && kept[r][k].hashCode() == text.hashCode()) {
                    content++;
                }
            }
        }
        int repooled = 0;
        int asked = 0;
        for (int r = 3; r < rounds; r += 4) {
            asked++;
            String b = built("gcj1st-d" + r + "-", 1);
            if (b.intern() == b) {
                repooled++;
            }
        }
        System.out.println("exact n=" + n + " rounds=" + rounds
                + " kept-identical=" + same + "/" + total
                + " kept-content=" + content + "/" + total);
        System.out.println("exact weak-cleared=" + weakCleared + "/" + weakTaken
                + " dropped-repooled-receiver=" + repooled + "/" + asked);
    }

    static void shared(int n) {
        String[] originals = new String[n];
        for (int i = 0; i < n; i++) {
            originals[i] = built("gcj1st-shared-", i);
        }
        System.gc();
        System.gc(); // the originals (and their value arrays) are old now
        String[] holder = new String[n];
        int receivers = 0;
        for (int i = 0; i < n; i++) {
            String copy = new String(originals[i]);
            if (copy.intern() == copy) {
                receivers++;
            }
            holder[i] = copy;
        }
        holder = null;
        churn(1024);
        System.gc();
        churn(16 * 1024);
        int intact = 0;
        for (int i = 0; i < n; i++) {
            String text = built("gcj1st-shared-", i);
            String pooled = text.intern();
            if (pooled.equals(text) && pooled.length() == text.length()
                    && pooled.hashCode() == text.hashCode()) {
                intact++;
            }
        }
        sink = originals;
        System.out.println("shared n=" + n + " young-receivers=" + receivers + "/" + n
                + " intact=" + intact + "/" + n);
    }
}
