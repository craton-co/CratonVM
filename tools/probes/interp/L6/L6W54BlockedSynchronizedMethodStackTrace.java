// Interpreter round i2 wave 54, lane L6: `Thread.getStackTrace()` of a
// thread blocked entering a contended `synchronized` method lists that
// method on top, as HotSpot does (it builds the frame before it locks); a
// `synchronized` block's entrant (the control, first and last) lists the
// method holding the block.
//
// CratonVM takes a synchronized method's monitor before it pushes the
// method's frame (`vm_exec::monitor_enter_synchronized_method`), so the
// blocked thread's trace starts at the CALLER (code reading). Recorded in
// docs/internal/fixed-bugs/interpreter-L6-a-blocked-synchronized-method-entrant-is-missing-from-its-stack-trace-FIXED-20261007.md.
//
// Canonical transcript: method names only (no line numbers, no JDK
// frames). Each trace is taken once the thread reads BLOCKED (3 s at most).
//
// HotSpot 25.0.3 (default flags, the Windows box):
//
//   synchronized block: viaBlock < lambda$main$0
//   static synchronized method: staticMethod < viaStatic
//   instance synchronized method: instanceMethod < viaInstance
//   synchronized block: viaBlock < lambda$main$2
//   done
public class L6W54BlockedSynchronizedMethodStackTrace {
    static volatile boolean before;

    static synchronized void staticMethod() {
        before = !before;
    }

    synchronized void instanceMethod() {
        before = !before;
    }

    static void viaStatic() {
        staticMethod();
    }

    static void viaInstance(L6W54BlockedSynchronizedMethodStackTrace target) {
        target.instanceMethod();
    }

    static void viaBlock(Object lock) {
        synchronized (lock) {
            before = !before;
        }
    }

    static void report(String label, Object lock, Runnable entry) throws InterruptedException {
        Thread t;
        synchronized (lock) {
            t = new Thread(entry, "entrant");
            t.setDaemon(true);
            t.start();
            long until = System.currentTimeMillis() + 3_000;
            while (t.getState() != Thread.State.BLOCKED && System.currentTimeMillis() < until) {
                Thread.sleep(1);
            }
            StackTraceElement[] trace = t.getStackTrace();
            StringBuilder top = new StringBuilder();
            for (int i = 0; i < trace.length && i < 2; i++) {
                if (i > 0) {
                    top.append(" < ");
                }
                top.append(trace[i].getMethodName());
            }
            System.out.println(label + ": " + top);
        }
        t.join(10_000);
    }

    public static void main(String[] args) throws Exception {
        Object block = new Object();
        L6W54BlockedSynchronizedMethodStackTrace target = new L6W54BlockedSynchronizedMethodStackTrace();
        report("synchronized block", block, () -> viaBlock(block));
        report("static synchronized method", L6W54BlockedSynchronizedMethodStackTrace.class,
                L6W54BlockedSynchronizedMethodStackTrace::viaStatic);
        report("instance synchronized method", target, () -> viaInstance(target));
        report("synchronized block", block, () -> viaBlock(block));
        System.out.println("done");
    }
}
