// Interpreter round i2 wave 55, lane L6: a thread suspended while it waits
// in `Object.wait`, then notified, does not take the monitor back while it
// stays suspended -- round 0 with `wait()`, round 1 (the same case again,
// warm) with a timed `wait(ms)`.
//
// `waiter-<r>` waits on `lock`; the debugger suspends it; `notifier-<r>`
// takes the lock, notifies and releases it; then `latecomer-<r>` enters the
// same lock. HotSpot's re-entry after a notify gives the monitor up while
// the thread has a suspension pending (`ObjectMonitor::wait`'s re-entry
// runs under `ExitOnSuspend`), so the latecomer gets the lock while the
// waiter stays suspended, reading MONITOR, and the waiter returns from
// `wait` only after its resume.
//
// CratonVM re-took the monitor inside the wait's re-acquisition and parked
// at the wait native's return holding it (`latecomer got it: false`,
// `waiter status: RUNNING`). Fixed in wave 55: the re-acquisition gives a
// fresh hold back while the thread is held suspended
// (`WaitReacquireReport::reentry_held_off`); `CRATONVM_FRAME_TRACE=1` prints
// `[MONITOR_EXIT_ON_SUSPEND] tid=<n> given_back=1` in each round (the
// positive control). And `currentContendedMonitor()` of the re-entering
// waiter is its lock (HotSpot's JDK-8256314 rule), which answered null
// (wave 55, `debug::inspect::monitor_reply`). Recorded in
// docs/internal/fixed-bugs/interpreter-L6-a-suspended-notified-waiter-retakes-its-monitor-FIXED-20261019.md.
//
// Canonical transcript: no ids, no addresses, no timings.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   ended
//
// Under a debugger:
//
//   javac -g -d out L6W55JdiSuspendedWaiterReentryTwice.java
//   cratonvm --java-home $JDK --jdwp-port 5802 -cp out L6W55JdiSuspendedWaiterReentryTwice wait
//   java -cp out L6W55JdiSuspendedWaiterReentryTwice debug 5802 > transcript.txt
//
// HotSpot 25.0.3 as the debuggee (default flags, the Windows box):
//
//   == attach
//   == round 0 (wait)
//     waiter status: WAIT
//     after the notify: latecomer got it: true waiter returned: false
//     waiter status: MONITOR suspended: true
//     waiter contended: java.lang.Object owned: 0
//     after the resume: waiter returned: true
//   == round 1 (timed wait)
//     waiter status: WAIT
//     after the notify: latecomer got it: true waiter returned: false
//     waiter status: MONITOR suspended: true
//     waiter contended: java.lang.Object owned: 0
//     after the resume: waiter returned: true
//   == end
//     disconnected
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;

public class L6W55JdiSuspendedWaiterReentryTwice {
    static final int ROUNDS = 2;
    /** Set by the debugger to round + 1: start the round, notify, start the latecomer, round done. */
    static volatile int go;
    static volatile int notifyAt;
    static volatile int lateAt;
    static volatile int doneAt;
    /** Set by the debuggee to round + 1 once the round's waiter waits. */
    static volatile int waitingAt;
    static final Object lock = new Object();
    static volatile boolean waiting;
    static volatile boolean waiterReturned;
    static volatile boolean latecomerGot;

    static void await(int round) {
        synchronized (lock) {
            waiting = true;
            try {
                if (round == 0) {
                    lock.wait();
                } else {
                    lock.wait(120_000);
                }
            } catch (InterruptedException e) {
                return;
            }
            waiterReturned = true;
        }
    }

    static void notifyIt() {
        synchronized (lock) {
            lock.notifyAll();
        }
    }

    static void late() {
        synchronized (lock) {
            latecomerGot = true;
        }
    }

    static Thread start(Runnable r, String name) {
        Thread t = new Thread(r, name);
        t.setDaemon(true);
        t.start();
        return t;
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean debugged = args.length == 1 && args[0].equals("wait");
        long until = System.currentTimeMillis() + 300_000;
        for (int r = 0; r < ROUNDS; r++) {
            final int round = r;
            while (debugged && go < round + 1 && System.currentTimeMillis() < until) {
                Thread.sleep(5);
            }
            waiting = false;
            waiterReturned = false;
            latecomerGot = false;
            Thread waiter = start(() -> await(round), "waiter-" + round);
            while (!waiting) {
                Thread.sleep(1);
            }
            // Until it really waits (it released the lock).
            synchronized (lock) {
                waitingAt = round + 1;
            }
            if (!debugged) {
                notifyAt = round + 1;
                lateAt = round + 1;
            }
            Thread notifier = null;
            Thread latecomer = null;
            while (System.currentTimeMillis() < until) {
                if (notifyAt >= round + 1 && notifier == null) {
                    notifier = start(L6W55JdiSuspendedWaiterReentryTwice::notifyIt, "notifier-" + round);
                }
                if (lateAt >= round + 1 && latecomer == null) {
                    latecomer = start(L6W55JdiSuspendedWaiterReentryTwice::late, "latecomer-" + round);
                }
                if (!debugged && latecomer != null) {
                    break;
                }
                if (doneAt >= round + 1) {
                    break;
                }
                Thread.sleep(5);
            }
            waiter.join(10_000);
            if (notifier != null) {
                notifier.join(10_000);
            }
            if (latecomer != null) {
                latecomer.join(10_000);
            }
        }
        System.out.println("ended");
    }

    static final class Debugger {
        static VirtualMachine vm;
        static ClassType type;

        static boolean flag(String name) {
            return ((BooleanValue) type.getValue(type.fieldByName(name))).value();
        }

        static int intField(String name) {
            return ((IntegerValue) type.getValue(type.fieldByName(name))).value();
        }

        static void set(String name, int value) {
            try {
                type.setValue(type.fieldByName(name), vm.mirrorOf(value));
            } catch (Exception e) {
                fail("set " + name + ": " + e);
            }
        }

        static String status(ThreadReference t) {
            switch (t.status()) {
                case ThreadReference.THREAD_STATUS_MONITOR: return "MONITOR";
                case ThreadReference.THREAD_STATUS_RUNNING: return "RUNNING";
                case ThreadReference.THREAD_STATUS_SLEEPING: return "SLEEPING";
                case ThreadReference.THREAD_STATUS_WAIT: return "WAIT";
                case ThreadReference.THREAD_STATUS_ZOMBIE: return "ZOMBIE";
                default: return "OTHER";
            }
        }

        static String monitorName(ThreadReference t) {
            try {
                ObjectReference m = t.currentContendedMonitor();
                return m == null ? "null" : m.referenceType().name();
            } catch (Exception e) {
                return e.getClass().getSimpleName();
            }
        }

        static String owned(ThreadReference t) {
            try {
                return String.valueOf(t.ownedMonitors().size());
            } catch (Exception e) {
                return e.getClass().getSimpleName();
            }
        }

        static ThreadReference thread(String name) {
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals(name)) {
                    return t;
                }
            }
            return null;
        }

        static void round(int r) throws Exception {
            System.out.println("== round " + r + (r == 0 ? " (wait)" : " (timed wait)"));
            set("go", r + 1);
            long until = System.currentTimeMillis() + 120_000;
            while (intField("waitingAt") < r + 1) {
                if (System.currentTimeMillis() > until) {
                    fail("the debuggee never started its waiter");
                }
                Thread.sleep(20);
            }
            ThreadReference waiter = thread("waiter-" + r);
            if (waiter == null) {
                fail("no waiter");
            }
            while (waiter.status() != ThreadReference.THREAD_STATUS_WAIT) {
                if (System.currentTimeMillis() > until) {
                    fail("the waiter never waited");
                }
                Thread.sleep(5);
            }
            System.out.println("  waiter status: " + status(waiter));
            waiter.suspend();
            set("notifyAt", r + 1);
            Thread.sleep(300);
            set("lateAt", r + 1);
            until = System.currentTimeMillis() + 2_000;
            while (!flag("latecomerGot") && System.currentTimeMillis() < until) {
                Thread.sleep(20);
            }
            System.out.println("  after the notify: latecomer got it: " + flag("latecomerGot")
                    + " waiter returned: " + flag("waiterReturned"));
            System.out.println("  waiter status: " + status(waiter)
                    + " suspended: " + waiter.isSuspended());
            System.out.println("  waiter contended: " + monitorName(waiter)
                    + " owned: " + owned(waiter));
            waiter.resume();
            until = System.currentTimeMillis() + 10_000;
            while (!flag("waiterReturned") && System.currentTimeMillis() < until) {
                Thread.sleep(20);
            }
            System.out.println("  after the resume: waiter returned: " + flag("waiterReturned"));
            set("doneAt", r + 1);
        }

        static void run(int port) throws Exception {
            AttachingConnector socket = null;
            for (AttachingConnector c : Bootstrap.virtualMachineManager().attachingConnectors()) {
                if (c.name().equals("com.sun.jdi.SocketAttach")) {
                    socket = c;
                }
            }
            Map<String, Connector.Argument> a = socket.defaultArguments();
            a.get("hostname").setValue("localhost");
            a.get("port").setValue(Integer.toString(port));
            a.get("timeout").setValue("60000");
            vm = attach(socket, a);
            System.out.println("== attach");
            type = (ClassType) awaitClass("L6W55JdiSuspendedWaiterReentryTwice");
            for (int r = 0; r < ROUNDS; r++) {
                round(r);
            }
            System.out.println("== end");
            boolean ended = false;
            while (!ended) {
                EventSet next;
                try {
                    next = vm.eventQueue().remove(60_000);
                } catch (VMDisconnectedException gone) {
                    break;
                }
                if (next == null) {
                    fail("the debuggee did not end");
                }
                for (Event e : next) {
                    if (e instanceof VMDisconnectEvent) {
                        ended = true;
                    }
                }
                if (!ended) {
                    try {
                        next.resume();
                    } catch (VMDisconnectedException gone) {
                        break;
                    }
                }
            }
            System.out.println("  disconnected");
        }

        static ReferenceType awaitClass(String name) throws Exception {
            long until = System.currentTimeMillis() + 120_000;
            while (true) {
                List<ReferenceType> found = vm.classesByName(name);
                if (!found.isEmpty() && found.get(0).isPrepared()) {
                    return found.get(0);
                }
                if (System.currentTimeMillis() > until) {
                    fail("class " + name + " never loaded");
                }
                Thread.sleep(50);
            }
        }

        static VirtualMachine attach(AttachingConnector socket, Map<String, Connector.Argument> a)
                throws Exception {
            long until = System.currentTimeMillis() + 120_000;
            while (true) {
                try {
                    return socket.attach(a);
                } catch (java.io.IOException notYet) {
                    if (System.currentTimeMillis() > until) {
                        throw notYet;
                    }
                    Thread.sleep(250);
                }
            }
        }

        static void fail(String why) {
            System.out.println("FAILED: " + why);
            try {
                vm.dispose();
            } catch (RuntimeException ignored) {
                // Already gone.
            }
            System.exit(2);
        }
    }
}
