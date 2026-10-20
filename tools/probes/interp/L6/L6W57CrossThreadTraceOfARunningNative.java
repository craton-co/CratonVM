// Interpreter round i2 wave 57, lane L6: `Thread.getStackTrace()` of
// ANOTHER thread while that thread runs a long native method outside a
// blocking region
// (docs/internal/fixed-bugs/interpreter-L6-a-cross-thread-stack-trace-waits-for-a-running-native-to-return-FIXED-20261024.md).
//
// `deflater` compresses 16 MiB at level 9 in one `Deflater.deflate` call
// (the `deflateBytesBytes` native runs for seconds and blocks nowhere) in a
// loop, bumping `calls` after each call. `main` takes its stack trace twice
// while it is inside such a call. HotSpot answers at once with the native
// on top, its Java callers below, and the deflater still in the same call.
//
// Expected output (HotSpot 25.0.3, default flags, the Windows box):
//
//   trace 0: deflateBytesBytes native=true < 2 JDK < deflate same call: true
//   trace 1: deflateBytesBytes native=true < 2 JDK < deflate same call: true
//   done
//
// CratonVM (wave 57, debug build, every mode): the frame-trace pause
// (`stw_publish_frame_traces`) waits for the deflater to return from the
// native -- ~5 s here -- and then lists it back in Java:
// `trace 0: deflate native=false same call: false` (default), `trace 0:
// deflate native=false < 1 JDK < deflate same call: false`
// (`--compatible --nojit`).
import java.util.zip.Deflater;

public class L6W57CrossThreadTraceOfARunningNative {
    static volatile boolean inCall;
    static volatile boolean stop;
    static volatile long calls;

    static void deflate() {
        byte[] in = new byte[16 << 20];
        long seed = 12345;
        for (int i = 0; i < in.length; i++) {
            seed = seed * 6364136223846793005L + 1442695040888963407L;
            // A small alphabet: compressible, so level 9 searches hard.
            in[i] = (byte) ('a' + (int) ((seed >>> 33) & 7));
        }
        byte[] out = new byte[in.length + (1 << 20)];
        Deflater d = new Deflater(Deflater.BEST_COMPRESSION);
        while (!stop) {
            d.reset();
            d.setInput(in);
            d.finish();
            inCall = true;
            d.deflate(out);
            inCall = false;
            calls++;
        }
        d.end();
    }

    static String describe(StackTraceElement[] trace) {
        if (trace.length == 0) {
            return "(empty)";
        }
        StringBuilder s = new StringBuilder(trace[0].getMethodName())
                .append(" native=").append(trace[0].isNativeMethod());
        int jdk = 0;
        for (int i = 1; i < trace.length; i++) {
            StackTraceElement e = trace[i];
            if (e.getClassName().startsWith("java.")) {
                jdk++;
                continue;
            }
            if (jdk > 0) {
                s.append(" < ").append(jdk).append(" JDK");
                jdk = 0;
            }
            s.append(" < ").append(e.getMethodName());
            if (e.getClassName().equals(L6W57CrossThreadTraceOfARunningNative.class.getName())) {
                break;
            }
        }
        return s.toString();
    }

    public static void main(String[] args) throws Exception {
        Thread t = new Thread(L6W57CrossThreadTraceOfARunningNative::deflate, "deflater");
        t.setDaemon(true);
        t.start();
        for (int k = 0; k < 2; k++) {
            long before = calls;
            while (!inCall || calls != before) {
                Thread.sleep(1);
                before = inCall ? before : calls;
            }
            // Well inside the call now.
            Thread.sleep(300);
            long c0 = calls;
            StackTraceElement[] trace = t.getStackTrace();
            boolean same = calls == c0 && inCall;
            System.out.println("trace " + k + ": " + describe(trace) + " same call: " + same);
            // Into the next call for the second trace.
            while (calls == c0) {
                Thread.sleep(5);
            }
        }
        stop = true;
        System.out.println("done");
    }
}
