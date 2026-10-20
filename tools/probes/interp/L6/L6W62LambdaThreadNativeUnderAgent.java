// Interpreter round i2 wave 62, lane L6: a thread started from a lambda
// (`new Thread(Class::method)`), running a long native, read from another
// thread while a C JVMTI agent is loaded
// (docs/internal/fixed-bugs/interpreter-L6-a-lambda-proxy-row-makes-a-running-native-unlisted-under-an-agent-FIXED-20261026.md).
//
// Run it with ANY -agentpath library (the defect needs a C JVMTI env bound:
// wave 60's lambda proxy rows exist only then), and also without one, in
// every mode and with --nojit (the defect showed under --nojit):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl6w61state.so tools/probes/interp/L6/L6W61JvmtiRunningNativeState.c
//   javac -d /tmp/l6w62lambda tools/probes/interp/L6/L6W62LambdaThreadNativeUnderAgent.java
//   java     -agentpath:/tmp/libl6w61state.so -cp /tmp/l6w62lambda L6W62LambdaThreadNativeUnderAgent
//   cratonvm -agentpath:/tmp/libl6w61state.so -cp /tmp/l6w62lambda L6W62LambdaThreadNativeUnderAgent
//
// `deflater` compresses 16 MiB at level 9 in one `Deflater.deflate` call, in
// a loop (`deflateBytesBytes` runs for seconds, in C, never blocking). Each
// round reads it three ways: `Thread.getStackTrace()` (its top four methods
// and its bottom one),
// JMX `getThreadInfo(id)` (depth 0) and `getThreadInfo(id, 3)`.
//
// Expected output (HotSpot 25.0.3, default flags, the Windows box; with the
// Rust port of the L6W61 shim as -agentpath and without an agent, the same
// lines):
//
//   round 1
//   trace: deflateBytesBytes(native) < deflate < deflate < deflate ... bottom=java.lang.Thread.run
//   depth 0: jmx=RUNNABLE inNative=true
//   depth 3: jmx=RUNNABLE inNative=true top=deflateBytesBytes native=true
//   round 2
//   trace: deflateBytesBytes(native) < deflate < deflate < deflate ... bottom=java.lang.Thread.run
//   depth 0: jmx=RUNNABLE inNative=true
//   depth 3: jmx=RUNNABLE inNative=true top=deflateBytesBytes native=true
//   done
//
// CratonVM before wave 62 (debug build, an agent loaded, --nojit): the trace
// waited for the native to return and read `deflate < deflate < deflate <
// run`, and both JMX lines read `inNative=false` (the lambda proxy row raised
// the thread's native-excuse word without `NX_LISTABLE`).
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.util.zip.Deflater;

public class L6W62LambdaThreadNativeUnderAgent {
    static volatile boolean inCall;

    static void deflate() {
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

    static String trace(Thread t) {
        StackTraceElement[] st = t.getStackTrace();
        StringBuilder sb = new StringBuilder("trace:");
        for (int i = 0; i < st.length && i < 4; i++) {
            sb.append(i == 0 ? " " : " < ").append(st[i].getMethodName());
            if (st[i].isNativeMethod()) {
                sb.append("(native)");
            }
        }
        if (st.length > 0) {
            StackTraceElement b = st[st.length - 1];
            sb.append(" ... bottom=").append(b.getClassName()).append('.').append(b.getMethodName());
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
        Thread t = new Thread(L6W62LambdaThreadNativeUnderAgent::deflate, "deflater");
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
