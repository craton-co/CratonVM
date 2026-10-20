// Interpreter round i1 wave 48, lane L1: the `InterruptedException` of an
// interrupted VIRTUAL-thread sleep, its message and its JDK frames
// (docs/internal/fixed-bugs/interpreter-L1-a-virtual-thread-sleep-interrupt-has-the-platform-shape-FIXED-20261012.md).
//
// HotSpot 25 (default flags, continuations on) sleeps a virtual thread in
// `VirtualThread.sleepNanos`, which throws `new InterruptedException()` (no
// message): the first such throw when the thread was interrupted before the
// sleep, the second after its park. Each row prints the message and the JDK
// frames of the trace (`java.*` entries with their lines, innermost first, up
// to the probe's own frame).
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L1W48VirtualSleepPath
//
// HotSpot 25.0.3, default flags, prints exactly (the JDK line numbers are
// 25.0.3's; the host compares against its own HotSpot):
//     pre sleep0 null at java.lang.VirtualThread.sleepNanos:971 java.lang.Thread.sleepNanos:507 java.lang.Thread.sleep:540
//     pre sleep null at java.lang.VirtualThread.sleepNanos:971 java.lang.Thread.sleepNanos:507 java.lang.Thread.sleep:540
//     mid sleep null at java.lang.VirtualThread.sleepNanos:982 java.lang.Thread.sleepNanos:507 java.lang.Thread.sleep:540
//     mid sleepDuration null at java.lang.VirtualThread.sleepNanos:982 java.lang.Thread.sleepNanos:507 java.lang.Thread.sleep:601
//
// This VM's virtual threads are `ThreadBuilders$BoundVirtualThread`s
// (`ContinuationSupport.isSupported0` answers false), whose `Thread.sleep`
// takes the platform branch into `Thread.sleepNanos0`, as HotSpot's do under
// `-XX:+UnlockExperimentalVMOptions -XX:-VMContinuations`, where every row
// reads `sleep interrupted at java.lang.Thread.sleepNanos0:-2
// java.lang.Thread.sleepNanos:509 java.lang.Thread.sleep:<540 or 601>`.
//
// CratonVM before wave 48 (Windows `livedbg`, JDK 25.0.3, every mode): the
// messages read `sleep interrupted`; the three `Thread.sleep(long)` rows
// listed no JDK frame and `sleepDuration` the platform chain. Since wave 48,
// with `CRATONVM_VIRTUAL_SLEEP_CONTINUATION_SHAPE` on (default on), every
// row matches HotSpot's default; `=0` restores the old rows (positive
// control).
import java.time.Duration;

public class L1W48VirtualSleepPath {
    interface Body {
        void run() throws Throwable;
    }

    static String outcome(Body b) {
        try {
            b.run();
            return "returned";
        } catch (Throwable t) {
            StringBuilder sb = new StringBuilder(t.getMessage() + " at");
            for (StackTraceElement e : t.getStackTrace()) {
                String c = e.getClassName();
                if (c.startsWith("L1W48VirtualSleepPath")) {
                    break;
                }
                sb.append(' ').append(c).append('.').append(e.getMethodName())
                        .append(':').append(e.getLineNumber());
            }
            return sb.toString();
        }
    }

    static String pre(Body body) throws Exception {
        String[] r = new String[1];
        Thread t = Thread.ofVirtual().start(() -> {
            Thread.currentThread().interrupt();
            r[0] = outcome(body);
        });
        t.join(10_000);
        return r[0];
    }

    static String mid(Body body) throws Exception {
        String[] r = new String[1];
        Thread t = Thread.ofVirtual().start(() -> r[0] = outcome(body));
        long end = System.nanoTime() + 5_000_000_000L;
        while (t.getState() != Thread.State.TIMED_WAITING && System.nanoTime() < end) {
            Thread.sleep(2);
        }
        Thread.sleep(20);
        t.interrupt();
        t.join(10_000);
        return r[0];
    }

    public static void main(String[] args) throws Exception {
        System.out.println("pre sleep0 " + pre(() -> Thread.sleep(0)));
        System.out.println("pre sleep " + pre(() -> Thread.sleep(60_000)));
        System.out.println("mid sleep " + mid(() -> Thread.sleep(60_000)));
        System.out.println("mid sleepDuration " + mid(() -> Thread.sleep(Duration.ofSeconds(60))));
    }
}
