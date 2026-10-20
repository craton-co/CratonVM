// Interpreter round i2 wave 60, lane L6: `Thread.getStackTrace()` of ANOTHER
// thread while that thread runs a long native entered under COMPILED code
// (docs/internal/fixed-bugs/interpreter-L6-a-cross-thread-stack-trace-waits-for-a-running-native-to-return-FIXED-20261024.md,
// docs/internal/fixed-bugs/interpreter-L6-proposal-list-a-native-threads-compiled-frames-by-a-brief-freeze-FIXED-20261024.md).
//
// Two shapes, each traced twice, in the order osr, warm, warm, osr:
//
// * osr: `deflateOsr` fills 16 MiB in a long loop (an OSR body by then) and
//   then calls `Deflater.deflate` from that loop's method, so compiled code
//   sits below the interpreted JDK frames that call the native;
// * warm: `deflateWarm` first calls `Deflater.deflate` 30 000 times on a
//   small input (the JDK callers of `deflateBytesBytes` are compiled by
//   then), then on 16 MiB, so the native is entered straight from compiled
//   code.
//
// `deflateBytesBytes` runs for seconds and blocks nowhere. HotSpot answers at
// once with the native on top, its Java callers below, and the worker still
// in the same call.
//
// Expected output (HotSpot 25.0.3, default flags, the Windows box):
//
//   osr 0: deflateBytesBytes native=true < 2 JDK < deflateOsr same call: true
//   warm 1: deflateBytesBytes native=true < 2 JDK < deflateWarm same call: true
//   warm 2: deflateBytesBytes native=true < 2 JDK < deflateWarm same call: true
//   osr 3: deflateBytesBytes native=true < 2 JDK < deflateOsr same call: true
//   done
//
// CratonVM before wave 60 (default JIT modes; `--nojit` already matched since
// wave 58): the frame-trace pause waits for the native to return and lists
// the worker back in Java (`osr 0: deflateOsr native=false ... same call:
// false`). With `CRATONVM_HANDSHAKE_NATIVE_EXCUSE_COMPILED` on,
// `CRATONVM_FRAME_TRACE=1` prints `[NATIVE_EXCUSE] trace tid=<n> depth=<n>
// native=true compiled chain=<n> entries=<n>` per trace.
import java.util.zip.Deflater;

public class L6W60CrossThreadTraceOfANativeUnderCompiledCode {
    static final class Worker implements Runnable {
        final boolean warm;
        volatile boolean inCall;
        volatile boolean stop;
        volatile long calls;

        Worker(boolean warm) {
            this.warm = warm;
        }

        public void run() {
            if (warm) {
                deflateWarm(this);
            } else {
                deflateOsr(this);
            }
        }
    }

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

    static void deflateOsr(Worker w) {
        byte[] in = new byte[16 << 20];
        long seed = 12345;
        for (int i = 0; i < in.length; i++) {
            seed = seed * 6364136223846793005L + 1442695040888963407L;
            in[i] = (byte) ('a' + (int) ((seed >>> 33) & 7));
        }
        byte[] out = new byte[in.length + (1 << 20)];
        Deflater d = new Deflater(Deflater.BEST_COMPRESSION);
        while (!w.stop) {
            d.reset();
            d.setInput(in);
            d.finish();
            w.inCall = true;
            d.deflate(out);
            w.inCall = false;
            w.calls++;
        }
        d.end();
    }

    static void deflateWarm(Worker w) {
        byte[] in = input();
        byte[] out = new byte[in.length + (1 << 20)];
        Deflater d = new Deflater(Deflater.BEST_COMPRESSION);
        byte[] small = new byte[64];
        long sink = 0;
        for (int k = 0; k < 30_000; k++) {
            d.reset();
            d.setInput(small);
            d.finish();
            sink += d.deflate(out);
        }
        if (sink < 0) {
            System.out.println("unreachable");
        }
        while (!w.stop) {
            d.reset();
            d.setInput(in);
            d.finish();
            w.inCall = true;
            d.deflate(out);
            w.inCall = false;
            w.calls++;
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
            if (e.getClassName().startsWith(L6W60CrossThreadTraceOfANativeUnderCompiledCode.class.getName())) {
                break;
            }
        }
        return s.toString();
    }

    static void traceOnce(String label, int k, boolean warm) throws Exception {
        Worker w = new Worker(warm);
        Thread t = new Thread(w, label);
        t.setDaemon(true);
        t.start();
        while (!w.inCall) {
            Thread.sleep(1);
        }
        // Well inside the call now.
        Thread.sleep(300);
        long c0 = w.calls;
        StackTraceElement[] trace = t.getStackTrace();
        boolean same = w.calls == c0 && w.inCall;
        System.out.println(label + " " + k + ": " + describe(trace) + " same call: " + same);
        w.stop = true;
        t.join();
    }

    public static void main(String[] args) throws Exception {
        traceOnce("osr", 0, false);
        traceOnce("warm", 1, true);
        traceOnce("warm", 2, true);
        traceOnce("osr", 3, false);
        System.out.println("done");
    }
}
