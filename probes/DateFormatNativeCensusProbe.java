/*
 * DateFormatNativeCensusProbe — how many NATIVE calls does one
 * `SimpleDateFormat.format(Date)` make?
 *
 * `DateFormatPathProbe` shows `SimpleDateFormat.format(Date)` at ~270x HotSpot
 * while `String.format("%t...")` — same fields, same Calendar — is within 1.6x,
 * and shows the two JIT tier-up exclusions (`java/util/**` receivers,
 * `invokespecial`) are BOTH inert on it. That points at the per-call native
 * funnel documented in
 * `native-call-funnel-is-the-per-call-floor-RETIRED-20260805.md`
 * (~330-810 ns per entry), not at compilation.
 *
 * Same technique as `LockNativeCensusProbe`: run with
 * `--dump-native-registry <out.json>` and do N formats and as close to nothing
 * else as a Java main can. Any native whose `invocations` count is a clean
 * multiple of N is on the format path, and the multiple IS the per-format call
 * count.
 *
 * argv[0] = iterations (default 20,000). Keep it small — this VM spends
 * ~150 us per format.
 */

import java.text.SimpleDateFormat;
import java.util.Date;
import java.util.Locale;
import java.util.TimeZone;

public final class DateFormatNativeCensusProbe {
    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 20_000;
        SimpleDateFormat sdf = new SimpleDateFormat("dd-MMM-yyyy HH:mm:ss", Locale.US);
        sdf.setTimeZone(TimeZone.getDefault());
        Date fixed = new Date(1_700_000_000_000L);
        String s = null;
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            s = sdf.format(fixed);
        }
        long d = System.nanoTime() - t0;
        System.out.printf(Locale.US, "%d formats in %.3f s (%.1f ns/format) last=%s%n",
                n, d / 1e9, d / (double) n, s);
        System.out.println("now read the census: any native with invocations ~= k*" + n
                + " is on the format path, k calls per format");
    }
}
