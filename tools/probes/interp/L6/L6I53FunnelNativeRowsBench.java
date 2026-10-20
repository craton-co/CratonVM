// Interpreter round i2, the i53 retirement: the per-call cost of the row
// record a native's entry makes for its readers
// (`jvm_thread::NativeRowsSnapshot`; docs/internal/fixed-bugs/interpreter-L6-a-native-below-a-reflective-call-is-listed-only-after-it-returns-FIXED-20261007.md).
//
// One funnel native (`Deflater.getAdler`, a field read behind the native
// call) called in a loop, in three places: `main` (no rows), a thread started
// from a `Runnable` (in `--compatible` a served `Thread.run` row), and inside
// a `Method.invoke` (a reflective row). Prints ns/call per row, median of 5.
import java.lang.reflect.Method;
import java.util.Arrays;
import java.util.zip.Deflater;

public class L6I53FunnelNativeRowsBench {
    static final int N = 2_000_000;
    static long sink;

    public static long loop() {
        Deflater d = new Deflater();
        long s = 0;
        for (int i = 0; i < N; i++) {
            s += d.getAdler();
        }
        d.end();
        return s;
    }

    static double time(Runnable r) {
        double[] t = new double[5];
        for (int k = 0; k < t.length; k++) {
            long start = System.nanoTime();
            r.run();
            t[k] = (System.nanoTime() - start) / (double) N;
        }
        Arrays.sort(t);
        return t[t.length / 2];
    }

    public static void main(String[] args) throws Exception {
        Method m = L6I53FunnelNativeRowsBench.class.getMethod("loop");
        double plain = time(() -> sink += loop());
        double[] threaded = new double[1];
        Thread t = new Thread(() -> threaded[0] = time(() -> sink += loop()));
        t.start();
        t.join();
        double reflective = time(() -> {
            try {
                sink += (Long) m.invoke(null);
            } catch (ReflectiveOperationException e) {
                throw new RuntimeException(e);
            }
        });
        System.out.printf("main           %8.2f ns/call%n", plain);
        System.out.printf("runnableThread %8.2f ns/call%n", threaded[0]);
        System.out.printf("underInvoke    %8.2f ns/call%n", reflective);
    }
}
