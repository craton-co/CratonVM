// Interpreter round i2 wave 54, lane L6: a thread suspended while it is
// blocked entering a contended monitor gives the monitor back when it wins
// it, and takes it only after its resume -- for a `synchronized` block, for
// a `synchronized` static method, and for the block again (a cold-VM
// first-call gap would show in the first section only).
//
// Each section: `holder-N` holds the section's lock; `blocked-N` blocks
// entering it; the debugger suspends `blocked-N`, lets `holder-N` release,
// and starts `latecomer-N`, which enters the same lock. HotSpot's contended
// entry gives the monitor up when the winner has a suspension pending
// (`ObjectMonitor::enter`, `ExitOnSuspend`), so the latecomer gets the lock
// while `blocked-N` stays suspended and `MONITOR`. Fixed in
// docs/internal/fixed-bugs/interpreter-L6-a-suspended-thread-takes-a-contended-monitor-FIXED-20261018.md.
//
// Canonical transcript: no ids, no addresses, no timings.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   ended
//
// Under a debugger:
//
//   javac -g -d out L6W54JdiSuspendedEntrantGivesTheMonitorBack.java
//   cratonvm --java-home $JDK --jdwp-port 5797 -cp out L6W54JdiSuspendedEntrantGivesTheMonitorBack wait
//   java -cp out L6W54JdiSuspendedEntrantGivesTheMonitorBack debug 5797 > transcript.txt
//
// HotSpot 25.0.3 as the debuggee (default flags, the Windows box):
//
//   == attach
//   == section 0 (synchronized block)
//     blocked status: MONITOR
//     after the release: latecomer got it: true blocked got it: false
//     blocked status: MONITOR suspended: true suspend count: 1
//     after the resume: blocked got it: true
//   == section 1 (synchronized method)
//     blocked status: MONITOR
//     after the release: latecomer got it: true blocked got it: false
//     blocked status: MONITOR suspended: true suspend count: 1
//     after the resume: blocked got it: true
//   == section 2 (synchronized block)
//     blocked status: MONITOR
//     after the release: latecomer got it: true blocked got it: false
//     blocked status: MONITOR suspended: true suspend count: 1
//     after the resume: blocked got it: true
//   == end
//     disconnected
//
// Before the fix CratonVM printed, in every section,
// `latecomer got it: false blocked got it: true` and `blocked status:
// RUNNING`. `CRATONVM_FRAME_TRACE=1` prints `[MONITOR_EXIT_ON_SUSPEND]
// tid=<n> given_back=1` once per section (the positive control);
// `CRATONVM_DEBUG_MONITOR_EXIT_ON_SUSPEND=0` restores the old transcript.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;

public class L6W54JdiSuspendedEntrantGivesTheMonitorBack {
    static final int SECTIONS = 3;
    /** Set by the debugger: section N runs once it reads N + 1. */
    static volatile int go;
    /** Set by the debuggee: section N's holder holds and its entrant runs. */
    static volatile int ready;
    /** Set by the debugger: section N's holder releases at N + 1. */
    static volatile int release;
    /** Set by the debugger: section N's latecomer starts at N + 1. */
    static volatile int late;
    /** Set by the debugger: section N is over at N + 1. */
    static volatile int done;
    static volatile boolean holding;
    static volatile boolean blockedGot;
    static volatile boolean latecomerGot;
    static final Object lockA = new Object();
    static final Object lockC = new Object();

    static Object blockLock(int section) {
        return section == 0 ? lockA : lockC;
    }

    static void hold(int section) {
        Object lock = section == 1 ? L6W54JdiSuspendedEntrantGivesTheMonitorBack.class : blockLock(section);
        synchronized (lock) {
            holding = true;
            while (release < section + 1) {
                try {
                    Thread.sleep(5);
                } catch (InterruptedException e) {
                    return;
                }
            }
        }
    }

    static void enterBlock(int section) {
        synchronized (blockLock(section)) {
            blockedGot = true;
        }
    }

    static synchronized void enterMethod() {
        blockedGot = true;
    }

    static synchronized void lateMethod() {
        latecomerGot = true;
    }

    static void enter(int section) {
        if (section == 1) {
            enterMethod();
        } else {
            enterBlock(section);
        }
    }

    static void lateEnter(int section) {
        if (section == 1) {
            lateMethod();
        } else {
            synchronized (blockLock(section)) {
                latecomerGot = true;
            }
        }
    }

    static void await(boolean debugged, java.util.function.BooleanSupplier until, long deadline)
            throws InterruptedException {
        while (debugged && !until.getAsBoolean() && System.currentTimeMillis() < deadline) {
            Thread.sleep(5);
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean debugged = args.length == 1 && args[0].equals("wait");
        long deadline = System.currentTimeMillis() + 300_000;
        for (int s = 0; s < SECTIONS; s++) {
            final int section = s;
            await(debugged, () -> go >= section + 1, deadline);
            holding = false;
            blockedGot = false;
            latecomerGot = false;
            if (!debugged) {
                release = section + 1;
            }
            Thread holder = new Thread(() -> hold(section), "holder-" + section);
            holder.setDaemon(true);
            holder.start();
            while (!holding && release < section + 1) {
                Thread.sleep(1);
            }
            Thread blocked = new Thread(() -> enter(section), "blocked-" + section);
            blocked.setDaemon(true);
            blocked.start();
            ready = section + 1;
            await(debugged, () -> late >= section + 1, deadline);
            Thread latecomer = new Thread(() -> lateEnter(section), "latecomer-" + section);
            latecomer.setDaemon(true);
            latecomer.start();
            await(debugged, () -> done >= section + 1, deadline);
            holder.join(10_000);
            blocked.join(10_000);
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

        static int intField(String name) {
            return ((IntegerValue) type.getValue(type.fieldByName(name))).value();
        }

        static void set(String name, int value) throws Exception {
            type.setValue(type.fieldByName(name), vm.mirrorOf(value));
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

        static void section(int s) throws Exception {
            System.out.println("== section " + s + (s == 1 ? " (synchronized method)" : " (synchronized block)"));
            set("go", s + 1);
            long until = System.currentTimeMillis() + 120_000;
            while (intField("ready") < s + 1) {
                if (System.currentTimeMillis() > until) {
                    fail("section " + s + " never started");
                }
                Thread.sleep(20);
            }
            ThreadReference blocked = thread("blocked-" + s);
            if (blocked == null) {
                fail("no blocked thread");
            }
            while (blocked.status() != ThreadReference.THREAD_STATUS_MONITOR) {
                if (System.currentTimeMillis() > until) {
                    fail("the blocked thread never blocked");
                }
                Thread.sleep(5);
            }
            System.out.println("  blocked status: " + status(blocked));
            blocked.suspend();
            set("release", s + 1);
            Thread.sleep(300);
            set("late", s + 1);
            until = System.currentTimeMillis() + 2_000;
            while (!flag("latecomerGot") && System.currentTimeMillis() < until) {
                Thread.sleep(20);
            }
            System.out.println("  after the release: latecomer got it: " + flag("latecomerGot")
                    + " blocked got it: " + flag("blockedGot"));
            System.out.println("  blocked status: " + status(blocked)
                    + " suspended: " + blocked.isSuspended()
                    + " suspend count: " + blocked.suspendCount());
            blocked.resume();
            until = System.currentTimeMillis() + 10_000;
            while (!flag("blockedGot") && System.currentTimeMillis() < until) {
                Thread.sleep(20);
            }
            System.out.println("  after the resume: blocked got it: " + flag("blockedGot"));
            ThreadReference holder = thread("holder-" + s);
            until = System.currentTimeMillis() + 10_000;
            while (holder != null && holder.status() != ThreadReference.THREAD_STATUS_ZOMBIE
                    && System.currentTimeMillis() < until) {
                Thread.sleep(20);
            }
            set("done", s + 1);
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
            type = (ClassType) awaitClass("L6W54JdiSuspendedEntrantGivesTheMonitorBack");
            for (int s = 0; s < SECTIONS; s++) {
                section(s);
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
