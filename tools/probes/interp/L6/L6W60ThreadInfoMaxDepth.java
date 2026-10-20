// Interpreter round i2 wave 60, lane L6: `ThreadMXBean.getThreadInfo(id)` /
// `getThreadInfo(id, maxDepth)` honour `maxDepth` and report no locked
// monitors (the `sun.management.ThreadImpl.getThreadInfo1` native), while
// `getThreadInfo(ids, true, true)` reports them
// (docs/internal/fixed-bugs/interpreter-L6-thread-info-ignores-max-depth-and-misses-a-running-native-FIXED-20261024.md).
//
// `holder` sleeps inside `synchronized (LOCK)`. The depth-0 line comes first
// and again last (cold-VM first-call gaps).
//
// Expected output (HotSpot 25.0.3, default flags, the Windows box):
//
//   getThreadInfo(id): frames=0 monitors=0 state=TIMED_WAITING
//   getThreadInfo(id, 1): frames=1 monitors=0
//   getThreadInfo(id, 4): frames=4 monitors=0
//   getThreadInfo(id, MAX): holder listed=true monitors=0
//   getThreadInfo(ids, true, true): holder listed=true monitors=1
//   getThreadInfo(ids, false, false): holder listed=true monitors=0
//   getThreadInfo(ids, true, true, 1): frames=1 monitors=0
//   getThreadInfo(id): frames=0 monitors=0 state=TIMED_WAITING
//   done
//
// CratonVM before wave 60: `getThreadInfo1` ignored `maxDepth` (every line
// listed the whole stack) and reported the held monitor on each, and
// `dumpThreads0` ignored its two booleans and `maxDepth` (`monitors=1`,
// the whole stack).
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;

public class L6W60ThreadInfoMaxDepth {
    static final Object LOCK = new Object();
    static volatile boolean holding;

    static void holder() {
        synchronized (LOCK) {
            holding = true;
            try {
                Thread.sleep(60_000);
            } catch (InterruptedException e) {
                // End of the probe.
            }
        }
    }

    static boolean lists(ThreadInfo ti, String method) {
        for (StackTraceElement e : ti.getStackTrace()) {
            if (e.getMethodName().equals(method)) {
                return true;
            }
        }
        return false;
    }

    public static void main(String[] args) throws Exception {
        Thread t = new Thread(L6W60ThreadInfoMaxDepth::holder, "holder");
        t.setDaemon(true);
        t.start();
        while (!holding || t.getState() != Thread.State.TIMED_WAITING) {
            Thread.sleep(5);
        }
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        ThreadInfo d0 = mx.getThreadInfo(t.threadId());
        System.out.println("getThreadInfo(id): frames=" + d0.getStackTrace().length
                + " monitors=" + d0.getLockedMonitors().length + " state=" + d0.getThreadState());
        ThreadInfo d1 = mx.getThreadInfo(t.threadId(), 1);
        System.out.println("getThreadInfo(id, 1): frames=" + d1.getStackTrace().length
                + " monitors=" + d1.getLockedMonitors().length);
        ThreadInfo d4 = mx.getThreadInfo(t.threadId(), 4);
        System.out.println("getThreadInfo(id, 4): frames=" + d4.getStackTrace().length
                + " monitors=" + d4.getLockedMonitors().length);
        ThreadInfo dm = mx.getThreadInfo(t.threadId(), Integer.MAX_VALUE);
        System.out.println("getThreadInfo(id, MAX): holder listed=" + lists(dm, "holder")
                + " monitors=" + dm.getLockedMonitors().length);
        ThreadInfo full = mx.getThreadInfo(new long[] {t.threadId()}, true, true)[0];
        System.out.println("getThreadInfo(ids, true, true): holder listed=" + lists(full, "holder")
                + " monitors=" + full.getLockedMonitors().length);
        ThreadInfo none = mx.getThreadInfo(new long[] {t.threadId()}, false, false)[0];
        System.out.println("getThreadInfo(ids, false, false): holder listed=" + lists(none, "holder")
                + " monitors=" + none.getLockedMonitors().length);
        ThreadInfo cut = mx.getThreadInfo(new long[] {t.threadId()}, true, true, 1)[0];
        System.out.println("getThreadInfo(ids, true, true, 1): frames=" + cut.getStackTrace().length
                + " monitors=" + cut.getLockedMonitors().length);
        ThreadInfo again = mx.getThreadInfo(t.threadId());
        System.out.println("getThreadInfo(id): frames=" + again.getStackTrace().length
                + " monitors=" + again.getLockedMonitors().length + " state=" + again.getThreadState());
        t.interrupt();
        System.out.println("done");
    }
}
