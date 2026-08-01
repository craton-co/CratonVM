import java.lang.ref.SoftReference;
import java.text.DateFormatSymbols;
import java.text.SimpleDateFormat;
import java.util.Date;
import java.util.Locale;

/**
 * Splits `SimpleDateFormat.format` (known-issue tomcat/32.4) into the pieces the
 * VM's own diagnostic pointed at.
 *
 * `CRATONVM_DBG_JIT_METHOD_STATS=1` on `DateFmtProbe` reports exactly ONE
 * `java/text/DateFormatSymbols.getProviderInstance` call per `format()` — but
 * the JDK caches `DateFormatSymbols` per locale behind a `SoftReference`, so a
 * warm process should call it approximately never. This probe asks which link
 * in that chain is broken:
 *
 *   A  DateFormatSymbols.getInstance(Locale.US)  -- the cached factory
 *   B  new DateFormatSymbols(Locale.US)          -- the uncached construction
 *   C  SoftReference.get() on a strongly-held referent -- does it survive?
 *   D  SimpleDateFormat.format, for scale
 */
public class DateSymbolsProbe {

    public static void main(String[] args) throws Exception {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 2000;

        // C first: a SoftReference whose referent is also strongly held must
        // never read back null. If it does, every JDK soft cache is dead and
        // the "cached" factory below is doing full work every call.
        Object strong = new Object();
        SoftReference<Object> ref = new SoftReference<>(strong);
        int cleared = 0;
        for (int i = 0; i < 100000; i++) {
            if (ref.get() == null) {
                cleared++;
            }
        }
        System.out.println("C SoftReference.get() returned null " + cleared + "/100000"
                + " (must be 0; referent is strongly reachable)");

        SimpleDateFormat sdf = new SimpleDateFormat("dd-MMM-yyyy HH:mm:ss", Locale.US);
        Date d = new Date();

        for (int round = 0; round < 3; round++) {
            System.out.println("--- round " + round + " (iters=" + iters + ") ---");

            long t0 = System.nanoTime();
            Object sink = null;
            for (int i = 0; i < iters; i++) {
                sink = DateFormatSymbols.getInstance(Locale.US);
            }
            long t1 = System.nanoTime();
            report("A DFS.getInstance    ", t1 - t0, iters);

            long t2 = System.nanoTime();
            for (int i = 0; i < iters; i++) {
                sink = new DateFormatSymbols(Locale.US);
            }
            long t3 = System.nanoTime();
            report("B new DFS(Locale)    ", t3 - t2, iters);

            long t4 = System.nanoTime();
            for (int i = 0; i < iters; i++) {
                sink = sdf.format(d);
            }
            long t5 = System.nanoTime();
            report("D SimpleDateFormat   ", t5 - t4, iters);

            if (sink == null) {
                System.out.println("unreachable");
            }
        }
        System.out.println("strong still held: " + (strong != null));
    }

    static void report(String label, long nanos, int iters) {
        System.out.println(label + " total=" + (nanos / 1000000L) + "ms  per-call="
                + (nanos / (double) iters / 1000.0) + "us");
    }
}
