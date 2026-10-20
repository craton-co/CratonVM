// Interpreter round i2 wave 60, lane L6: JMX's `ThreadInfo.isInNative()` of
// a thread running a long native outside any blocking region
// (docs/internal/fixed-bugs/interpreter-L6-thread-info-ignores-max-depth-and-misses-a-running-native-FIXED-20261024.md).
//
// `deflater` compresses 16 MiB at level 9 in one `Deflater.deflate` call (the
// `deflateBytesBytes` native runs for seconds). HotSpot runs it in
// `_thread_in_native`: RUNNABLE, and in native for JMX, with or without a
// stack trace asked for.
//
// Expected output (HotSpot 25.0.3, default flags, the Windows box):
//
//   depth 0: state=RUNNABLE jmx=RUNNABLE inNative=true
//   depth 3: state=RUNNABLE jmx=RUNNABLE inNative=true top=deflateBytesBytes native=true
//   depth 0: state=RUNNABLE jmx=RUNNABLE inNative=true
//   done
//
// CratonVM before wave 60 (debug build, jdk-only and --compatible):
// `inNative=false` on every line, and the depth-0 lines listed a stack
// (`top=deflateBytesBytes native=true`).
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.util.zip.Deflater;

public class L6W60RunningNativeIsInNative {
    static volatile boolean inCall;

    static void deflate() {
        byte[] in = new byte[16 << 20];
        long seed = 12345;
        for (int i = 0; i < in.length; i++) {
            seed = seed * 6364136223846793005L + 1442695040888963407L;
            in[i] = (byte) ('a' + (int) ((seed >>> 33) & 7));
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

    static String line(Thread t, int depth) {
        ThreadInfo ti = depth == 0
                ? ManagementFactory.getThreadMXBean().getThreadInfo(t.threadId())
                : ManagementFactory.getThreadMXBean().getThreadInfo(t.threadId(), depth);
        String s = "depth " + depth + ": state=" + t.getState() + " jmx=" + ti.getThreadState()
                + " inNative=" + ti.isInNative();
        StackTraceElement[] st = ti.getStackTrace();
        if (st.length > 0) {
            s += " top=" + st[0].getMethodName() + " native=" + st[0].isNativeMethod();
        }
        return s;
    }

    public static void main(String[] args) throws Exception {
        Thread t = new Thread(L6W60RunningNativeIsInNative::deflate, "deflater");
        t.setDaemon(true);
        t.start();
        while (!inCall) {
            Thread.sleep(1);
        }
        Thread.sleep(300);
        System.out.println(line(t, 0));
        System.out.println(line(t, 3));
        System.out.println(line(t, 0));
        System.out.println("done");
    }
}
