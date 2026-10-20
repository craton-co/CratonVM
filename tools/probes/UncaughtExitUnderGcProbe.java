import java.util.ArrayList;

/**
 * gc-common w11-f: a program that dies on an uncaught exception while the
 * collector is busy must still EXIT, with rc=1, promptly.
 *
 * `common-w10v-uncaught-error-exit-can-hang-in-process-exit` (now FIXED): the
 * launcher's error arm called `std::process::exit(1)` on the `main-vm` thread.
 * On Windows that is `ExitProcess`, which kills every other thread (G1's
 * markers, pause workers, a daemon's collection) and THEN runs the calling
 * thread's thread-local destructors, among them the collector's per-thread
 * barrier buffers, whose `Drop` takes locks those killed threads may have held.
 * 3 runs in 8 hung after printing the whole trailer.
 *
 * Shape: daemon threads churn short-lived arrays (young collections, card and
 * SATB traffic from several threads), main fills half the heap with live
 * arrays (so G1 crosses its IHOP and opens a concurrent mark), then prints an
 * UNTERMINATED line and throws.
 *
 * Expected, on every collector and on HotSpot: stdout `PROBE-THROWING` (no
 * newline, so it also checks the stdout flush on the error arm), the
 * `Exception in thread "main" java.lang.IllegalStateException` trailer on
 * stderr, rc=1, within a few seconds. Ends on its own; the only way it does
 * not is the hang this probe exists to catch. Run it 20 times per collector:
 *
 *   for i in $(seq 20); do cratonvm -Xmx256m -XX:+UseG1GC UncaughtExitUnderGcProbe; echo " rc=$?"; done
 *
 * Optional argument: the number of churning daemon threads (default 2).
 */
public class UncaughtExitUnderGcProbe {
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
        System.out.print("PROBE-THROWING");
        throw new IllegalStateException("w11f: uncaught exit under a busy collector");
    }
}
