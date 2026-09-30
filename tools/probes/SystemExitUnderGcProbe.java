import java.util.ArrayList;

/**
 * gc-common w11-f, the `System.exit` twin of {@code UncaughtExitUnderGcProbe}:
 * a program that calls {@code System.exit(3)} while the collector is busy must
 * exit, with rc=3, promptly.
 *
 * Shape: daemon threads churn short-lived arrays (young collections, card and
 * SATB traffic from several threads), main fills half the heap with live
 * arrays (so G1 crosses its IHOP and opens a concurrent mark), prints an
 * UNTERMINATED line and exits.
 *
 * Expected, on every collector and on HotSpot: stdout `PROBE-EXITING` (no
 * newline, so it also checks the stdout flush on the exit path), rc=3, within
 * a few seconds. It is also one of the G1 rows of
 * `docs/internal/gc/g1-concurrent-mark-reads-corrupt-value-cells-with-the-jit-on-FIXED-20260930.md`:
 * the run must not print `corrupt Value cell`.
 *
 *   for i in $(seq 20); do cratonvm -Xmx256m -XX:+UseG1GC SystemExitUnderGcProbe; echo " rc=$?"; done
 *
 * Optional argument: the number of churning daemon threads (default 2).
 */
public class SystemExitUnderGcProbe {
    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        int churners = args.length > 0 ? Integer.parseInt(args[0]) : 2;
        for (int t = 0; t < churners; t++) {
            Thread th = new Thread(() -> {
                ArrayList<byte[]> keep = new ArrayList<>();
                long n = 0;
                while (true) {
                    keep.add(new byte[1024 + (int) (n++ % 64) * 512]);
                    if (keep.size() > 256) {
                        keep.subList(0, 128).clear();
                    }
                    sink = keep;
                }
            }, "churn-" + t);
            th.setDaemon(true);
            th.start();
        }
        ArrayList<byte[]> old = new ArrayList<>();
        long target = Runtime.getRuntime().maxMemory() / 2;
        for (long have = 0; have < target; have += 64 * 1024) {
            old.add(new byte[64 * 1024]);
        }
        sink = old;
        Thread.sleep(300);
        System.out.print("PROBE-EXITING");
        System.exit(3);
    }
}
