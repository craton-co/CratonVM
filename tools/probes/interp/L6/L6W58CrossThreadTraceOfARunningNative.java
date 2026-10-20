// Interpreter round i2 wave 58, lane L6: another thread's stack trace while
// that thread runs a long native method outside a blocking region, called
// from INTERPRETED code (the hot fill loop is in a method of its own, so the
// caller of `Deflater.deflate` is not an OSR body)
// (docs/internal/fixed-bugs/interpreter-L6-a-cross-thread-stack-trace-waits-for-a-running-native-to-return-FIXED-20261024.md).
//
// `deflater` compresses 16 MiB at level 9 in one `Deflater.deflate` call
// (the `deflateBytesBytes` native runs for seconds and blocks nowhere) in a
// loop, bumping `calls` after each call. `main` asks for its stack three
// ways, each while it is inside a fresh such call -- `Thread.getStackTrace()`,
// `Thread.getAllStackTraces()`, `ThreadMXBean.getThreadInfo(id, depth)` --
// and then the first way again. HotSpot answers at once with
// the native on top, its Java callers below, the deflater still in the same
// call.
//
// Expected output (HotSpot 25.0.3, default flags, the Windows box):
//
//   getStackTrace: deflateBytesBytes native=true < 2 JDK < deflateLoop same call: true
//   getAllStackTraces: deflateBytesBytes native=true < 2 JDK < deflateLoop same call: true
//   getThreadInfo: deflateBytesBytes native=true < 2 JDK < deflateLoop same call: true
//   getStackTrace again: deflateBytesBytes native=true < 2 JDK < deflateLoop same call: true
//   done
//
// CratonVM before wave 58 (and with CRATONVM_HANDSHAKE_NATIVE_EXCUSE=0): each
// request takes the frame-trace pause, which waits for the native to return
// (seconds), and lists the deflater back in Java with `same call: false`.
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;
import java.util.zip.Deflater;

public class L6W58CrossThreadTraceOfARunningNative {
    static volatile boolean inCall;
    static volatile boolean stop;
    static volatile long calls;

    static byte[] input() {
        byte[] in = new byte[16 << 20];
        long seed = 12345;
        for (int i = 0; i < in.length; i++) {
            seed = seed * 6364136223846793005L + 1442695040888963407L;
            // A small alphabet: compressible, so level 9 searches hard.
            in[i] = (byte) ('a' + (int) ((seed >>> 33) & 7));
        }
        return in;
    }

    static void deflateLoop() {
        byte[] in = input();
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
        if (trace == null || trace.length == 0) {
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
            if (e.getClassName().equals(L6W58CrossThreadTraceOfARunningNative.class.getName())) {
                break;
            }
        }
        return s.toString();
    }

    /** Waits until the deflater is well inside a fresh call; returns its count. */
    static long insideACall() throws InterruptedException {
        long before = calls;
        while (!inCall || calls != before) {
            Thread.sleep(1);
            before = inCall ? before : calls;
        }
        Thread.sleep(300);
        return calls;
    }

    static void report(String how, StackTraceElement[] trace, long c0) {
        boolean same = calls == c0 && inCall;
        System.out.println(how + ": " + describe(trace) + " same call: " + same);
    }

    public static void main(String[] args) throws Exception {
        Thread t = new Thread(L6W58CrossThreadTraceOfARunningNative::deflateLoop, "deflater");
        t.setDaemon(true);
        t.start();
        // Loaded before any call is timed.
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        long c = insideACall();
        report("getStackTrace", t.getStackTrace(), c);
        c = nextCall(c);
        report("getAllStackTraces", Thread.getAllStackTraces().get(t), c);
        c = nextCall(c);
        ThreadInfo info = mx.getThreadInfo(t.threadId(), 16);
        report("getThreadInfo", info == null ? null : info.getStackTrace(), c);
        c = nextCall(c);
        report("getStackTrace again", t.getStackTrace(), c);
        stop = true;
        System.out.println("done");
    }

    /** Waits for the call counted `c` to end, then inside the next one. */
    static long nextCall(long c) throws InterruptedException {
        while (calls == c) {
            Thread.sleep(5);
        }
        return insideACall();
    }
}
