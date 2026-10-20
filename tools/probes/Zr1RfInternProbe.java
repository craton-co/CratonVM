import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;

/**
 * zr z1/rf (2026-10-12): the interned-String table on ZGC against HotSpot 25
 * {@code -XX:+UseZGC} (the ZGC half of
 * {@code gcg-g2w-proposal-weak-string-table-on-g1-and-zgc-20261001};
 * CratonVM switch {@code CRATONVM_ZGC_WEAK_STRING_TABLE=1}).
 *
 * <ul>
 *   <li>{@code weakclear}: a {@code WeakReference} to a computed, interned String
 *       nothing else references clears at the next {@code System.gc()} (HotSpot's
 *       StringTable is weak on every collector); one the program still holds
 *       does not, and {@code intern()} of its text keeps answering the same object
 *       across 20 collections whose churn makes the ZGC slide move it.</li>
 *   <li>{@code churn [n]}: interns {@code n} (default 3,000,000) distinct computed
 *       Strings in a {@code -Xmx64m} heap, keeping every 50,000th; the strong pool
 *       runs out of memory, the weak one does not, and every kept String still is
 *       its own {@code intern()} at the end.</li>
 * </ul>
 *
 * <p>stdout is deterministic and identical on HotSpot 25; stderr: elapsed ms.
 */
public class Zr1RfInternProbe {
    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "weakclear";
        long t0 = System.nanoTime();
        switch (mode) {
            case "weakclear" -> weakClear();
            case "churn" -> churn(args.length > 1 ? Integer.parseInt(args[1]) : 3_000_000);
            default -> throw new IllegalArgumentException(mode);
        }
        System.err.println("elapsed-ms=" + (System.nanoTime() - t0) / 1_000_000);
        System.out.println("PROBE-OK " + mode);
    }

    static String built(String a, int i) {
        return new StringBuilder(a).append(i).toString();
    }

    static WeakReference<String> internAndForget(int i) {
        String s = built("zr1rf-dropped-", i).intern();
        return new WeakReference<>(s);
    }

    static void weakClear() {
        WeakReference<String> dropped = internAndForget(7);
        String held = built("zr1rf-held-", 7).intern();
        WeakReference<String> heldRef = new WeakReference<>(held);
        boolean identity = true;
        for (int round = 0; round < 20; round++) {
            // Garbage in front of `held` so a compacting cycle moves it.
            List<byte[]> junk = new ArrayList<>();
            for (int k = 0; k < 64; k++) {
                junk.add(new byte[16 * 1024]);
            }
            sink = junk;
            sink = null;
            System.gc();
            identity &= built("zr1rf-held-", 7).intern() == held;
        }
        System.out.println("dropped-intern-weak-cleared=" + (dropped.get() == null));
        System.out.println("held-intern-weak-kept=" + (heldRef.get() == held));
        System.out.println("held-intern-identity=" + identity);
    }

    static void churn(int n) {
        List<String> kept = new ArrayList<>();
        for (int i = 0; i < n; i++) {
            String s = built("zr1rf-churn-", i).intern();
            if (i % 50_000 == 0) {
                kept.add(s);
            }
        }
        boolean identity = true;
        for (String s : kept) {
            identity &= new StringBuilder(s).toString().intern() == s;
        }
        System.out.println("churn-kept=" + kept.size());
        System.out.println("churn-identity=" + identity);
    }
}
