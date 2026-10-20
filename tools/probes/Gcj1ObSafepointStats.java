/**
 * gcj j1/ob (residual 8 of
 * `gci-i47w-j47x-xlog-safepoint-residuals-after-i44sp`): run with
 * {@code -Xlog:safepoint+stats:stdout:none}. Three {@code System.gc()} calls
 * and almost no allocation, so HotSpot 25 Serial runs three
 * {@code SerialGCCollect} safepoints: its stdout is the statistics header,
 * three rows, this probe's line, then the exit summary ({@code Halt},
 * {@code SerialGCCollect}, the two maxima).
 *
 * <p>The rows carry nanosecond timings and thread counts, so the row compares
 * stdout after {@code s/[0-9]+/N/g; s/ +/ /g} (the report gives the exact
 * rule); the header line is compared verbatim.
 */
public class Gcj1ObSafepointStats {
    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 3;
        for (int i = 0; i < n; i++) {
            System.gc();
        }
        System.out.println("gcj1ob safepoint-stats gcs=" + n);
    }
}
