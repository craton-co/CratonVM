// Interpreter round i2 wave 56, lane L6: `Thread.getStackTrace()` of a
// thread blocked entering a contended `synchronized` method lists that
// method on top, as HotSpot does (it builds the frame before it locks), on
// the paths wave 55 left in the old order (stage 2 of
// `CRATONVM_SYNC_METHOD_FRAME_FIRST`):
//
//  * a Thread subclass's `synchronized run()`, entered from the VM
//    (`interpreter::execute`, a Rust-to-Java entry);
//  * a reflective call of a `static synchronized` method;
//  * a COMPILED caller's call (a hot `compiledCaller`, whose call site the
//    JIT took over) -- the call reaches the by-name path, `execute`;
//  * a COMPILED callee entered from a cold, interpreted caller
//    (`jit_bridge::execute_jit_call`, the compiled-body door).
//
// The first two run cold first and again at the end (cold-VM gaps).
// `CRATONVM_FRAME_TRACE=1` prints `[SYNC_FRAME_FIRST] tid=<n> <method>` for
// each blocked entry served frame first, and `[SYNC_FRAME_FIRST] tid=<n>
// compiled door declined` when a compiled-body door declined to it (the
// positive controls). Recorded in
// docs/internal/fixed-bugs/interpreter-L6-a-blocked-synchronized-method-entrant-is-missing-from-its-stack-trace-FIXED-20261007.md
//
// Canonical transcript: method names only (no line numbers, no JDK
// frames): the top frame, then the nearest frame of this class below it.
// Each trace is taken once the thread reads BLOCKED (3 s at most).
//
// HotSpot 25.0.3 (default flags, the Windows box):
//
//   run cold: run < -
//   reflect cold: reflectTarget < viaReflection
//   compiled caller: compiledTarget < compiledCaller
//   compiled callee: hotTarget < viaColdCaller
//   run again: run < -
//   reflect again: reflectTarget < viaReflection
//   counted: 100006
//   done
import java.lang.reflect.Method;

public class L6W56BlockedSynchronizedEntrantStageTwo {
    static long count;

    static final class SyncRun extends Thread {
        SyncRun() {
            super("entrant-run");
            setDaemon(true);
        }

        @Override
        public synchronized void run() {
            count++;
        }
    }

    static synchronized void reflectTarget() {
        count++;
    }

    static synchronized void compiledTarget(int by) {
        count += by;
    }

    static synchronized void hotTarget(int by) {
        count += by;
    }

    static void viaReflection() {
        try {
            Method m = L6W56BlockedSynchronizedEntrantStageTwo.class.getDeclaredMethod("reflectTarget");
            m.invoke(null);
        } catch (ReflectiveOperationException e) {
            throw new RuntimeException(e);
        }
    }

    static void compiledCaller(int by) {
        compiledTarget(by);
    }

    static void warmHotTarget() {
        for (int i = 0; i < 50_000; i++) {
            hotTarget(1);
        }
    }

    static void viaColdCaller() {
        hotTarget(1);
    }

    static String describe(StackTraceElement[] trace) {
        if (trace.length == 0) {
            return "(empty)";
        }
        String below = "-";
        for (int i = 1; i < trace.length; i++) {
            if (trace[i].getClassName().equals(L6W56BlockedSynchronizedEntrantStageTwo.class.getName())) {
                below = trace[i].getMethodName();
                break;
            }
        }
        return trace[0].getMethodName() + " < " + below;
    }

    static void awaitBlocked(Thread t) throws InterruptedException {
        long until = System.currentTimeMillis() + 3_000;
        while (t.getState() != Thread.State.BLOCKED && System.currentTimeMillis() < until) {
            Thread.sleep(1);
        }
    }

    static void report(String label, Object lock, Runnable entry) throws InterruptedException {
        Thread t;
        synchronized (lock) {
            t = new Thread(entry, "entrant");
            t.setDaemon(true);
            t.start();
            awaitBlocked(t);
            System.out.println(label + ": " + describe(t.getStackTrace()));
        }
        t.join(10_000);
    }

    static void reportRun(String label) throws InterruptedException {
        SyncRun t = new SyncRun();
        synchronized (t) {
            t.start();
            awaitBlocked(t);
            System.out.println(label + ": " + describe(t.getStackTrace()));
        }
        t.join(10_000);
    }

    public static void main(String[] args) throws Exception {
        Class<?> me = L6W56BlockedSynchronizedEntrantStageTwo.class;
        reportRun("run cold");
        report("reflect cold", me, L6W56BlockedSynchronizedEntrantStageTwo::viaReflection);
        // Make `compiledCaller` (and its call site) hot, uncontended.
        for (int i = 0; i < 50_000; i++) {
            compiledCaller(1);
        }
        report("compiled caller", me, () -> compiledCaller(1));
        // Make `hotTarget` hot from its own loop; `viaColdCaller` never ran.
        warmHotTarget();
        report("compiled callee", me, L6W56BlockedSynchronizedEntrantStageTwo::viaColdCaller);
        reportRun("run again");
        report("reflect again", me, L6W56BlockedSynchronizedEntrantStageTwo::viaReflection);
        System.out.println("counted: " + count);
        System.out.println("done");
    }
}
