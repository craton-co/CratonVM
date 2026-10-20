// Interpreter round i2 wave 61, lane L6: another thread's stack trace of a
// thread inside a long Java method that `--compatible` serves from a
// registered native (`Arrays.fill(int[], int)`), and JMX's view of it
// (`CRATONVM_HANDSHAKE_STAND_IN_ROWS`).
//
// `filler` fills a 16M-int array in a loop. HotSpot runs the Java, so the
// trace lists `Arrays.fill` on top (not native) above the probe's `fill`, and
// JMX says it is not in native. CratonVM under `--compatible` runs a native in
// its place and listed no row for it: `fill < run`, the probe's `fill` on top.
//
// Expected output (HotSpot 25.0.3, default flags, the Windows box; three
// runs, the same each time):
//
//   trace: java.util.Arrays.fill native=false < StandIn.fill
//   jmx: inNative=false java.util.Arrays.fill native=false < StandIn.fill
//   (the two lines three times)
//   done
//
// CratonVM before wave 61 (debug build, `--compatible` and `--compatible
// --nojit`): `trace: L6W61StandInOnTopOfAnotherThreadsTrace.fill native=false
// < java.lang.Thread.run` (jdk-only runs the bytecode and matched).
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.util.Arrays;

public class L6W61StandInOnTopOfAnotherThreadsTrace {
    static volatile boolean inCall;

    static void fill() {
        int[] a = new int[16 << 20];
        int v = 0;
        while (true) {
            inCall = true;
            Arrays.fill(a, v++);
        }
    }

    static String name(StackTraceElement e) {
        String c = e.getClassName().equals(L6W61StandInOnTopOfAnotherThreadsTrace.class.getName())
                ? "StandIn" : e.getClassName();
        return c + "." + e.getMethodName();
    }

    static String top2(StackTraceElement[] st) {
        if (st.length == 0) {
            return "<empty>";
        }
        return name(st[0]) + " native=" + st[0].isNativeMethod() + (st.length > 1 ? " < " + name(st[1]) : "");
    }

    public static void main(String[] args) throws Exception {
        Thread t = new Thread(L6W61StandInOnTopOfAnotherThreadsTrace::fill, "filler");
        t.setDaemon(true);
        t.start();
        while (!inCall) {
            Thread.sleep(1);
        }
        Thread.sleep(300);
        for (int k = 0; k < 3; k++) {
            System.out.println("trace: " + top2(t.getStackTrace()));
            ThreadInfo ti = ManagementFactory.getThreadMXBean().getThreadInfo(t.threadId(), 2);
            System.out.println("jmx: inNative=" + ti.isInNative() + " " + top2(ti.getStackTrace()));
        }
        System.out.println("done");
    }
}
