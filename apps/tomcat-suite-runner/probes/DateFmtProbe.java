import java.text.SimpleDateFormat;
import java.util.Date;
import java.util.Locale;
import java.util.TimeZone;

/**
 * Isolates the pathological path behind
 * org.apache.juli.TestOneLineFormatterPerformance.testDateFormat:
 * on CratonVM DateFormatCache (SimpleDateFormat-backed) is ~1000x slower than
 * HotSpot, while String.format is only ~2x slower.
 */
public class DateFmtProbe {

    static int iters = Integer.getInteger("probe.iters", 20000).intValue();

    public static void main(String[] args) throws Exception {
        long now = System.currentTimeMillis();

        // Warm up everything once.
        SimpleDateFormat sdf = new SimpleDateFormat("dd-MMM-yyyy HH:mm:ss", Locale.US);
        sdf.setTimeZone(TimeZone.getDefault());
        Date d = new Date();

        for (int round = 0; round < 3; round++) {
            System.out.println("--- round " + round + " (iters=" + iters + ") ---");

            // A: String.format with %t conversions (the "naive" path the test compares against)
            long t0 = System.nanoTime();
            String sA = null;
            for (int i = 0; i < iters; i++) {
                sA = String.format("%1$td-%1$tb-%1$tY %1$tH:%1$tM:%1$tS", Long.valueOf(now + i));
            }
            long t1 = System.nanoTime();
            report("A String.format      ", t1 - t0, sA);

            // B: SimpleDateFormat.format(Date) — the DateFormatCache miss path
            long t2 = System.nanoTime();
            String sB = null;
            for (int i = 0; i < iters; i++) {
                d.setTime(now + i * 1000L);
                sB = sdf.format(d);
            }
            long t3 = System.nanoTime();
            report("B SimpleDateFormat   ", t3 - t2, sB);

            // C: SimpleDateFormat.format on the SAME date (no field recompute change)
            long t4 = System.nanoTime();
            String sC = null;
            for (int i = 0; i < iters; i++) {
                sC = sdf.format(d);
            }
            long t5 = System.nanoTime();
            report("C SDF same date      ", t5 - t4, sC);

            // D: raw Calendar field computation, no formatting
            java.util.Calendar cal = java.util.Calendar.getInstance(Locale.US);
            long t6 = System.nanoTime();
            int acc = 0;
            for (int i = 0; i < iters; i++) {
                cal.setTimeInMillis(now + i * 1000L);
                acc += cal.get(java.util.Calendar.SECOND);
            }
            long t7 = System.nanoTime();
            report("D Calendar.get       ", t7 - t6, "acc=" + acc);

            // E: new Date() + toString (control for object churn)
            long t8 = System.nanoTime();
            String sE = null;
            for (int i = 0; i < iters; i++) {
                sE = new StringBuilder().append(now + i).toString();
            }
            long t9 = System.nanoTime();
            report("E StringBuilder      ", t9 - t8, sE);
        }
    }

    static void report(String label, long nanos, Object sample) {
        System.out.println(label + " total=" + (nanos / 1000000L) + "ms  per-call="
                + (nanos / (double) iters / 1000.0) + "us  sample=" + sample);
    }
}
