// Interpreter round i2 wave 55, lane L6: `Thread.getStackTrace()` of a
// thread blocked entering a contended `synchronized` method lists that
// method on top, as HotSpot does (it builds the frame before it locks) --
// each shape first on a cold call site and again once the site has run a
// few thousand uncontended calls (its invoke cache and door are warm), and
// a synchronized method whose FIRST instruction calls another synchronized
// method (the entrant is in `outer`, not yet in `inner`).
//
// CratonVM took a synchronized method's monitor before it pushed the
// method's frame, so the blocked thread's trace started at the CALLER.
// `CRATONVM_SYNC_METHOD_FRAME_FIRST` (wave 55) pushes the frame first;
// `CRATONVM_FRAME_TRACE=1` prints `[SYNC_FRAME_FIRST] tid=<n> <method>` for
// each blocked entry it served (the positive control). Recorded in
// docs/internal/fixed-bugs/interpreter-L6-a-blocked-synchronized-method-entrant-is-missing-from-its-stack-trace-FIXED-20261007.md
// (narrowed in wave 55: a compiled caller keeps the old order, so the warm
// loop stays short enough for the call sites to stay interpreted).
//
// Canonical transcript: method names only (no line numbers, no JDK
// frames). Each trace is taken once the thread reads BLOCKED (3 s at most).
//
// HotSpot 25.0.3 (default flags, the Windows box):
//
//   static cold: staticMethod < viaStatic
//   instance cold: instanceMethod < viaInstance
//   outer cold: outer < viaOuter
//   block: viaBlock < lambda$main$1
//   static warm: staticMethod < viaStatic
//   instance warm: instanceMethod < viaInstance
//   outer warm: outer < viaOuter
//   counted: 300
//   done
public class L6W55BlockedSynchronizedMethodTraceTwice {
    static long count;

    static synchronized void staticMethod() {
        count++;
    }

    synchronized void instanceMethod(int by) {
        count += by;
    }

    static synchronized void inner() {
        count++;
    }

    static synchronized void outer() {
        inner();
    }

    static void viaStatic() {
        staticMethod();
    }

    static void viaInstance(L6W55BlockedSynchronizedMethodTraceTwice target) {
        target.instanceMethod(1);
    }

    static void viaOuter() {
        outer();
    }

    static void viaBlock(Object lock) {
        synchronized (lock) {
            count++;
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
        Class<?> me = L6W55BlockedSynchronizedMethodTraceTwice.class;
        L6W55BlockedSynchronizedMethodTraceTwice target = new L6W55BlockedSynchronizedMethodTraceTwice();
        Object block = new Object();
        report("static cold", me, L6W55BlockedSynchronizedMethodTraceTwice::viaStatic);
        report("instance cold", target, () -> viaInstance(target));
        report("outer cold", me, L6W55BlockedSynchronizedMethodTraceTwice::viaOuter);
        report("block", block, () -> viaBlock(block));
        // Warm the same call sites, uncontended.
        for (int i = 0; i < 100; i++) {
            viaStatic();
            viaInstance(target);
            viaOuter();
        }
        report("static warm", me, L6W55BlockedSynchronizedMethodTraceTwice::viaStatic);
        report("instance warm", target, () -> viaInstance(target));
        report("outer warm", me, L6W55BlockedSynchronizedMethodTraceTwice::viaOuter);
        System.out.println("counted: " + (count - 7));
        System.out.println("done");
    }
}
