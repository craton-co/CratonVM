// Interpreter round i2 wave 54, lane L6: a thread suspended while it waits
// in `Object.wait`, then notified, does not take the monitor back while it
// stays suspended.
//
// `waiter` waits on `lock`; the debugger suspends it; `notifier` takes the
// lock, notifies and releases it; then `latecomer` enters the same lock.
// HotSpot's re-entry after a notify gives the monitor up when the thread
// has a suspension pending (`ObjectMonitor::wait`'s re-entry runs under
// `ExitOnSuspend`, as a contended `enter` does), so `latecomer` gets the
// lock while `waiter` stays suspended, and `waiter` returns from `wait`
// only after its resume. Recorded in (fixed in wave 55)
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
//   javac -g -d out L6W54JdiSuspendedWaiterReentry.java
//   cratonvm --java-home $JDK --jdwp-port 5800 -cp out L6W54JdiSuspendedWaiterReentry wait
//   java -cp out L6W54JdiSuspendedWaiterReentry debug 5800 > transcript.txt
//
// HotSpot 25.0.3 as the debuggee (default flags, the Windows box):
//
//   == attach
//     waiter status: WAIT
//     after the notify: latecomer got it: true waiter returned: false
//     waiter status: MONITOR suspended: true
//     after the resume: waiter returned: true
//   == end
//     disconnected
//
// CratonVM (code reading, wave 54): the notified waiter re-takes the monitor
// inside `MonitorTable::wait_outcome` and parks at the native's return
// holding it, so the expected CratonVM lines are `latecomer got it: false
// waiter returned: false` and `waiter status: RUNNING suspended: true`.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;

public class L6W54JdiSuspendedWaiterReentry {
    /**
     * Set by the debugger: 1 = start, 4 = notify, 5 = start the latecomer,
     * 3 = done. Set to 2 by the debuggee once the waiter waits.
     */
    static volatile int phase;
    static final Object lock = new Object();
    static volatile boolean waiting;
    static volatile boolean waiterReturned;
    static volatile boolean latecomerGot;

    static void await() {
        synchronized (lock) {
            waiting = true;
            try {
                lock.wait();
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

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean debugged = args.length == 1 && args[0].equals("wait");
        long until = System.currentTimeMillis() + 300_000;
        while (debugged && phase == 0 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        Thread waiter = new Thread(L6W54JdiSuspendedWaiterReentry::await, "waiter");
        waiter.setDaemon(true);
        waiter.start();
        while (!waiting) {
            Thread.sleep(1);
        }
        // Until it really waits (it released the lock).
        synchronized (lock) {
            phase = Math.max(phase, 2);
        }
        if (!debugged) {
            phase = 4;
        }
        Thread notifier = null;
        Thread latecomer = null;
        while (System.currentTimeMillis() < until) {
            if (phase >= 4 && notifier == null) {
                notifier = new Thread(L6W54JdiSuspendedWaiterReentry::notifyIt, "notifier");
                notifier.setDaemon(true);
                notifier.start();
                if (!debugged) {
                    break;
                }
            }
            if (phase == 5 && latecomer == null) {
                latecomer = new Thread(L6W54JdiSuspendedWaiterReentry::late, "latecomer");
                latecomer.setDaemon(true);
                latecomer.start();
            }
            if (phase == 3) {
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
        System.out.println("ended");
    }

    static final class Debugger {
        static VirtualMachine vm;
        static ClassType type;

        static boolean flag(String name) {
            return ((BooleanValue) type.getValue(type.fieldByName(name))).value();
        }

        static int phase() {
            return ((IntegerValue) type.getValue(type.fieldByName("phase"))).value();
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

        static ThreadReference thread(String name) {
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals(name)) {
                    return t;
                }
            }
            return null;
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
            type = (ClassType) awaitClass("L6W54JdiSuspendedWaiterReentry");
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(1));
            long until = System.currentTimeMillis() + 120_000;
            while (phase() < 2) {
                if (System.currentTimeMillis() > until) {
                    fail("the debuggee never started its waiter");
                }
                Thread.sleep(20);
            }
            ThreadReference waiter = thread("waiter");
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
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(4));
            Thread.sleep(300);
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(5));
            until = System.currentTimeMillis() + 2_000;
            while (!flag("latecomerGot") && System.currentTimeMillis() < until) {
                Thread.sleep(20);
            }
            System.out.println("  after the notify: latecomer got it: " + flag("latecomerGot")
                    + " waiter returned: " + flag("waiterReturned"));
            System.out.println("  waiter status: " + status(waiter)
                    + " suspended: " + waiter.isSuspended());
            waiter.resume();
            until = System.currentTimeMillis() + 10_000;
            while (!flag("waiterReturned") && System.currentTimeMillis() < until) {
                Thread.sleep(20);
            }
            System.out.println("  after the resume: waiter returned: " + flag("waiterReturned"));
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(3));
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
