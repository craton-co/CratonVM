/*
 * DateFormatCostProbe — price the layers under
 * `org.apache.juli.TestOneLineFormatterPerformance.testDateFormat`.
 *
 * That test is a RATIO test: it asserts `DateFormatCache` beats
 * `String.format("%1$td-%1$tb-%1$tY %1$tH:%1$tM:%1$tS", ...)` over 1,000,000
 * iterations. A uniformly slow VM still passes it. CratonVM fails it because
 * the two sides are not uniformly slow:
 *
 *     HotSpot   String.format 2.01 s   DateFormatCache 0.103 s   (19.5x faster)
 *     CratonVM  String.format 3.56 s   DateFormatCache 162.9 s   (46x SLOWER)
 *
 * There is also a feedback loop in the test worth knowing about before reading
 * any number here: it feeds `System.nanoTime()` in as if it were millis, so
 * `DateFormatCache`'s key is `nanos / 1000` — MICROseconds. On a VM where an
 * iteration costs ~100 ns the key barely moves and nearly every call hits the
 * `seconds == previousSeconds` fast path; on a VM where an iteration costs
 * ~100 µs the key moves every time and every call is a miss. Slow makes it
 * slower. So this probe measures the layers SEPARATELY and with a FIXED
 * timestamp, where the hit/miss split is the same on both VMs.
 *
 *   javac -cp "<tomcat suite cp>" -d <out> probes/DateFormatCostProbe.java
 *   <vm> -cp "<out>:<tomcat suite cp>" DateFormatCostProbe [iters]
 */

import java.text.FieldPosition;
import java.text.SimpleDateFormat;
import java.util.Date;
import java.util.Locale;
import java.util.TimeZone;

import org.apache.juli.DateFormatCache;

public class DateFormatCostProbe {

    private static final String PATTERN = "dd-MMM-yyyy HH:mm:ss";

    public static void main(String[] args) throws Exception {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;
        // Warm every path before timing any of it.
        run(Math.min(iters / 10, 20_000), false);
        run(iters, true);
    }

    private static void run(int iters, boolean report) throws Exception {
        SimpleDateFormat sdf = new SimpleDateFormat(PATTERN, Locale.US);
        sdf.setTimeZone(TimeZone.getDefault());
        Date fixed = new Date(1_700_000_000_000L);
        StringBuffer sb = new StringBuffer(32);
        FieldPosition fp = new FieldPosition(0);

        // 1. SimpleDateFormat.format(Date) — what DateFormatCache calls on a miss.
        long t0 = System.nanoTime();
        String last = null;
        for (int i = 0; i < iters; i++) {
            last = sdf.format(fixed);
        }
        long sdfNs = System.nanoTime() - t0;

        // 2. The same via the StringBuffer overload, so the result String
        //    allocation is out of the picture.
        t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) {
            sb.setLength(0);
            sdf.format(fixed, sb, fp);
        }
        long sdfBufNs = System.nanoTime() - t0;

        // 3. String.format with the %t conversions — the test's OTHER arm.
        t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) {
            last = String.format("%1$td-%1$tb-%1$tY %1$tH:%1$tM:%1$tS", Long.valueOf(fixed.getTime()));
        }
        long strFmtNs = System.nanoTime() - t0;

        // 4. DateFormatCache, ALL HITS (constant timestamp) — the cache's own
        //    overhead with no formatting underneath.
        DateFormatCache cache = new DateFormatCache(5, PATTERN, null);
        t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) {
            last = cache.getFormat(1_700_000_000_000L);
        }
        long cacheHitNs = System.nanoTime() - t0;

        // 5. DateFormatCache, ALL MISSES (timestamp advances one second per
        //    call, past the 5-entry window) — the shape the failing test
        //    degenerates into.
        DateFormatCache cache2 = new DateFormatCache(5, PATTERN, null);
        t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) {
            last = cache2.getFormat(1_700_000_000_000L + i * 1000L);
        }
        long cacheMissNs = System.nanoTime() - t0;

        if (!report) {
            return;
        }
        System.out.println("iters=" + iters + " (last=" + last + ")");
        row("SimpleDateFormat.format(Date)", sdfNs, iters);
        row("SimpleDateFormat.format(Date,StringBuffer,FieldPosition)", sdfBufNs, iters);
        row("String.format(%t...)", strFmtNs, iters);
        row("DateFormatCache.getFormat  ALL HITS", cacheHitNs, iters);
        row("DateFormatCache.getFormat  ALL MISSES", cacheMissNs, iters);
        System.out.println("PROBE-RATIO strFormat/cacheMiss = " +
                String.format(Locale.US, "%.2f", (double) strFmtNs / (double) cacheMissNs) +
                "   (the test needs cache < strFormat, i.e. ratio > 1)");
    }

    private static void row(String name, long ns, int iters) {
        System.out.println(String.format(Locale.US, "PROBE %-58s %10.3f ms   %9.1f ns/op", name,
                ns / 1e6, (double) ns / iters));
    }
}
