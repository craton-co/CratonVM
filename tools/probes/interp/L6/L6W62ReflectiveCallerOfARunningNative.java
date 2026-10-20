// Interpreter round i2 wave 62, lane L6: a thread running a long native
// reached through `Method.invoke`, read from another thread
// (docs/internal/fixed-bugs/interpreter-L6-a-native-below-a-reflective-call-is-listed-only-after-it-returns-FIXED-20261007.md).
//
// `deflater` calls `deflate()` reflectively; it compresses 16 MiB at level 9
// in one `Deflater.deflate` call, in a loop (`deflateBytesBytes` runs for
// seconds, in C, never blocking). Each round reads it with
// `Thread.getStackTrace()` (the frames from the native down to the
// reflective caller `viaReflection`, the JDK's reflection frames between
// counted, not named) and JMX `getThreadInfo(id)` / `getThreadInfo(id, 3)`.
//
// Expected output: see the HotSpot block below (measured, HotSpot 25.0.3,
// default flags, the Windows box, two runs, the same each time).
//
//   round 1
//   trace: deflateBytesBytes(native) < deflate < deflate < deflate < [2 JDK] < viaReflection
//   depth 0: jmx=RUNNABLE inNative=true
//   depth 3: jmx=RUNNABLE inNative=true top=deflateBytesBytes native=true
//   round 2
//   trace: deflateBytesBytes(native) < deflate < deflate < deflate < [2 JDK] < viaReflection
//   depth 0: jmx=RUNNABLE inNative=true
//   depth 3: jmx=RUNNABLE inNative=true top=deflateBytesBytes native=true
//   done
//
// CratonVM, wave 62 (debug build, measured): every read waits
// for the native to return; `--nojit` prints `trace: deflate < deflate <
// deflate < [2 JDK] < viaReflection` and `inNative=false` on both JMX rows
// (the default mode and --compatible also lose the two Deflater frames).
// `CRATONVM_HANDSHAKE_NATIVE_CALLBACK=1` fixes the `depth 0` rows only.
// Fixed by the i53 retirement (2026-10-07): HotSpot's output in all four
// modes (the pause splices the reflective calls the thread recorded).
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.lang.reflect.Method;
import java.util.zip.Deflater;

public class L6W62ReflectiveCallerOfARunningNative {
    static volatile boolean inCall;

    public static void deflate() {
        // 1 MiB of pseudo-random letters, copied 16 times (each copy lies
        // beyond deflate's 32 KiB window, so it compresses as slowly as the
        // first); generated quickly even by an interpreter.
        byte[] in = new byte[16 << 20];
        long seed = 12345;
        for (int i = 0; i < (1 << 20); i++) {
            seed = seed * 6364136223846793005L + 1442695040888963407L;
            in[i] = (byte) ('a' + (int) ((seed >>> 33) & 7));
        }
        for (int k = 1; k < 16; k++) {
            System.arraycopy(in, 0, in, k << 20, 1 << 20);
        }
        byte[] out = new byte[in.length + (1 << 20)];
        Deflater d = new Deflater(Deflater.BEST_COMPRESSION);
        while (true) {
            d.reset();
            d.setInput(in);
            d.finish();
            inCall = true;
            d.deflate(out);
        }
    }

    static void viaReflection() {
        try {
            Method m = L6W62ReflectiveCallerOfARunningNative.class.getMethod("deflate");
            m.invoke(null);
        } catch (ReflectiveOperationException e) {
            System.out.println("reflection failed: " + e);
        }
    }

    static String trace(Thread t) {
        StackTraceElement[] st = t.getStackTrace();
        StringBuilder sb = new StringBuilder("trace:");
        long jdk = 0;
        boolean first = true;
        for (StackTraceElement e : st) {
            String c = e.getClassName();
            if (c.startsWith("java.") || c.startsWith("jdk.")) {
                if (!c.equals("java.util.zip.Deflater")) {
                    jdk++;
                    continue;
                }
            }
            if (jdk > 0) {
                sb.append(" < [").append(jdk).append(" JDK]");
                jdk = 0;
            }
            sb.append(first ? " " : " < ").append(e.getMethodName());
            if (e.isNativeMethod()) {
                sb.append("(native)");
            }
            first = false;
            if (e.getMethodName().equals("viaReflection")) {
                break;
            }
        }
        return sb.toString();
    }

    static String jmx(Thread t, int depth) {
        ThreadInfo ti = depth == 0
                ? ManagementFactory.getThreadMXBean().getThreadInfo(t.threadId())
                : ManagementFactory.getThreadMXBean().getThreadInfo(t.threadId(), depth);
        String s = "depth " + depth + ": jmx=" + ti.getThreadState() + " inNative=" + ti.isInNative();
        StackTraceElement[] st = ti.getStackTrace();
        if (st.length > 0) {
            s += " top=" + st[0].getMethodName() + " native=" + st[0].isNativeMethod();
        }
        return s;
    }

    public static void main(String[] args) throws Exception {
        Thread t = new Thread(L6W62ReflectiveCallerOfARunningNative::viaReflection, "deflater");
        t.setDaemon(true);
        t.start();
        while (!inCall) {
            Thread.sleep(1);
        }
        for (long round = 1; round <= 2; round++) {
            Thread.sleep(300);
            System.out.println("round " + round);
            System.out.println(trace(t));
            System.out.println(jmx(t, 0));
            System.out.println(jmx(t, 3));
        }
        System.out.println("done");
    }
}
