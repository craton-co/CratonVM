import java.time.ZoneId;
import java.util.TimeZone;

/**
 * Regression witness for the duplicate `java/util/TimeZone.getDefault()`
 * native registration (native-builtins/src/lib.rs, commit 98878a6dd,
 * 2026-07-31): a second `registry.register("java/util/TimeZone",
 * "getDefault", ...)` call shadowed the earlier, correct one (last
 * registration wins on a native's (class, method, descriptor) triple), so
 * `TimeZone.getDefault()` stopped tracking `TimeZone.setDefault(...)` and
 * always reported the VM's *startup* zone instead.
 *
 * That silently broke every caller that reads the settable JVM default —
 * `ZoneId.systemDefault()` and any JDBC/native code that consults
 * `TimeZone.getDefault()` directly — which is exactly the shape Hibernate's
 * `Timezones.withDefaultTimeZone()` test helper exercises: set a default
 * zone, then read it back from a freshly spawned thread. It produced 60
 * timezone-offset-sized failures in `OffsetDateTimeTest` alone, identically
 * under the JIT and `--nojit` (native-registration bug, not a JIT/GC one).
 *
 * Expected output on both HotSpot and a correct CratonVM: every "after"/
 * "in new thread" line shows GMT-08:00, matching what was just set.
 */
public class TimeZoneDefaultTrackingProbe {
    public static void main(String[] args) throws Exception {
        TimeZone before = TimeZone.getDefault();
        System.out.println("before setDefault: TimeZone.getDefault()=" + before.getID()
                + " ZoneId.systemDefault()=" + ZoneId.systemDefault());

        TimeZone.setDefault(TimeZone.getTimeZone("GMT-08:00"));
        String mainTz = TimeZone.getDefault().getID();
        String mainZone = ZoneId.systemDefault().toString();
        System.out.println("after setDefault (main thread): TimeZone.getDefault()=" + mainTz
                + " ZoneId.systemDefault()=" + mainZone);

        // Timezones.withDefaultTimeZone() (Hibernate's temporal test helper)
        // runs the actual assertions on a freshly spawned thread specifically
        // to catch thread-local caching bugs; mirror that here.
        String[] threadTz = new String[1];
        String[] threadZone = new String[1];
        Thread t = new Thread(() -> {
            threadTz[0] = TimeZone.getDefault().getID();
            threadZone[0] = ZoneId.systemDefault().toString();
        });
        t.start();
        t.join();
        System.out.println("in new thread: TimeZone.getDefault()=" + threadTz[0]
                + " ZoneId.systemDefault()=" + threadZone[0]);

        boolean pass = "GMT-08:00".equals(mainTz) && "GMT-08:00".equals(mainZone)
                && "GMT-08:00".equals(threadTz[0]) && "GMT-08:00".equals(threadZone[0]);
        System.out.println(pass ? "@@RESULT PASS" : "@@RESULT FAIL");
        if (!pass) {
            System.exit(1);
        }
    }
}
