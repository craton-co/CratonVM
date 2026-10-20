// Interpreter round i2 wave 54, lane L6: `Thread.getState()` of a thread
// blocked entering a contended `synchronized` METHOD is BLOCKED, whatever
// the thread blocked on before.
//
// CratonVM stored the Java blocking state (`GcBlockState::java_state`,
// which `Thread.getState()` and JDWP `ThreadReference.Status` read while a
// thread is in its blocked region) only for a `monitorenter`
// (`vm_exec::monitor_enter_blocking`), never for an `ACC_SYNCHRONIZED`
// method's entry (`vm_exec::monitor_enter_synchronized_method`): such a
// thread reported whatever its last blocking call left there -- RUNNABLE
// for a fresh thread, TIMED_WAITING after a `sleep` or a timed `wait`,
// WAITING after a `wait()`. Fixed in wave 54 (lane L6).
//
// Canonical transcript: no ids, no addresses, no timings. Each case is
// printed once its thread reads BLOCKED or after 3 s, whichever is first.
//
// HotSpot 25.0.3 (default flags, the Windows box):
//
//   static method, fresh thread: BLOCKED
//   instance method, after a sleep: BLOCKED
//   instance method, after a timed wait: BLOCKED
//   instance method, after a wait: BLOCKED
//   synchronized block, after a sleep: BLOCKED
//   static method, fresh thread: BLOCKED
//   done
public class L6W54SynchronizedMethodEntrantState {
    static volatile boolean before;
    static volatile boolean woken;

    static synchronized void staticMethod() {
        before = !before;
    }

    synchronized void instanceMethod() {
        before = !before;
    }

    interface Prelude {
        void run() throws InterruptedException;
    }

    static void report(String label, Object lock, Prelude prelude, Runnable entry)
            throws InterruptedException {
        Thread t;
        synchronized (lock) {
            t = new Thread(() -> {
                try {
                    prelude.run();
                } catch (InterruptedException e) {
                    return;
                }
                entry.run();
            }, "entrant");
            t.setDaemon(true);
            t.start();
            long until = System.currentTimeMillis() + 3_000;
            while (t.getState() != Thread.State.BLOCKED && System.currentTimeMillis() < until) {
                Thread.sleep(1);
            }
            System.out.println(label + ": " + t.getState());
        }
        t.join(10_000);
    }

    public static void main(String[] args) throws Exception {
        Class<?> cls = L6W54SynchronizedMethodEntrantState.class;
        L6W54SynchronizedMethodEntrantState target = new L6W54SynchronizedMethodEntrantState();
        Object own = new Object();
        report("static method, fresh thread", cls, () -> { }, L6W54SynchronizedMethodEntrantState::staticMethod);
        report("instance method, after a sleep", target, () -> Thread.sleep(2), target::instanceMethod);
        report("instance method, after a timed wait", target, () -> {
            synchronized (own) {
                own.wait(2);
            }
        }, target::instanceMethod);
        report("instance method, after a wait", target, () -> {
            Thread waker = new Thread(() -> {
                try {
                    Thread.sleep(5);
                } catch (InterruptedException e) {
                    return;
                }
                synchronized (own) {
                    woken = true;
                    own.notifyAll();
                }
            });
            woken = false;
            waker.start();
            synchronized (own) {
                while (!woken) {
                    own.wait();
                }
            }
            waker.join();
        }, target::instanceMethod);
        Object block = new Object();
        report("synchronized block, after a sleep", block, () -> Thread.sleep(2), () -> {
            synchronized (block) {
                before = !before;
            }
        });
        report("static method, fresh thread", cls, () -> { }, L6W54SynchronizedMethodEntrantState::staticMethod);
        System.out.println("done");
    }
}
