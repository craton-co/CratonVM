// Interpreter round i2 wave 53, lane L6: a thread suspended while it is
// blocked entering a contended monitor does not take the monitor while it
// stays suspended.
//
// `holder` holds `lock`; `blocked` blocks entering it; the debugger suspends
// `blocked` (`ThreadReference.suspend()`), lets `holder` release the lock,
// and then starts `latecomer`, which enters the same lock. HotSpot's
// monitor entry gives the monitor up again when the thread that won it has
// a suspension pending (`ObjectMonitor`'s `ExitOnSuspend`), so `latecomer`
// gets the lock while `blocked` stays suspended, and `blocked` takes it only
// after its resume. Recorded in
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
//   javac -g -d out L6W53JdiSuspendedThreadAndContendedMonitor.java
//   cratonvm --java-home $JDK --jdwp-port 5796 -cp out L6W53JdiSuspendedThreadAndContendedMonitor wait
//   java -cp out L6W53JdiSuspendedThreadAndContendedMonitor debug 5796 > transcript.txt
//
// HotSpot 25.0.3 as the debuggee (default flags, the Windows box):
//
//   == attach
//     blocked status: MONITOR
//     after the release: latecomer got it: true blocked got it: false
//     blocked status: MONITOR suspended: true
//     after the resume: blocked got it: true
//   == end
//     disconnected
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;

public class L6W53JdiSuspendedThreadAndContendedMonitor {
    /**
     * Set by the debugger: 1 = start, 4 = the holder releases, 5 = start the
     * latecomer, 3 = done. Set to 2 by the debuggee once the holder holds the
     * lock and the blocked thread runs.
     */
    static volatile int phase;
    static final Object lock = new Object();
    static volatile boolean holding;
    static volatile boolean blockedGot;
    static volatile boolean latecomerGot;

    static void hold() {
        synchronized (lock) {
            holding = true;
            while (phase < 4) {
                try {
                    Thread.sleep(5);
                } catch (InterruptedException e) {
                    return;
                }
            }
        }
    }

    static void block() {
        synchronized (lock) {
            blockedGot = true;
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
        if (!debugged) {
            phase = 4;
        }
        Thread holder = new Thread(L6W53JdiSuspendedThreadAndContendedMonitor::hold, "holder");
        holder.setDaemon(true);
        holder.start();
        while (!holding && phase < 4) {
            Thread.sleep(1);
        }
        Thread blocked = new Thread(L6W53JdiSuspendedThreadAndContendedMonitor::block, "blocked");
        blocked.setDaemon(true);
        blocked.start();
        if (debugged) {
            phase = 2;
        }
        Thread latecomer = null;
        while (debugged && phase != 3 && System.currentTimeMillis() < until) {
            Thread.sleep(5);
            if (phase == 5 && latecomer == null) {
                latecomer = new Thread(L6W53JdiSuspendedThreadAndContendedMonitor::late, "latecomer");
                latecomer.setDaemon(true);
                latecomer.start();
            }
        }
        holder.join(10_000);
        blocked.join(10_000);
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
            type = (ClassType) awaitClass("L6W53JdiSuspendedThreadAndContendedMonitor");
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(1));
            long until = System.currentTimeMillis() + 120_000;
            while (((IntegerValue) type.getValue(type.fieldByName("phase"))).value() < 2) {
                if (System.currentTimeMillis() > until) {
                    fail("the debuggee never started its threads");
                }
                Thread.sleep(20);
            }
            ThreadReference blocked = null;
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals("blocked")) {
                    blocked = t;
                }
            }
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
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(4));
            Thread.sleep(300);
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(5));
            until = System.currentTimeMillis() + 2_000;
            while (!flag("latecomerGot") && System.currentTimeMillis() < until) {
                Thread.sleep(20);
            }
            System.out.println("  after the release: latecomer got it: " + flag("latecomerGot")
                    + " blocked got it: " + flag("blockedGot"));
            System.out.println("  blocked status: " + status(blocked)
                    + " suspended: " + blocked.isSuspended());
            blocked.resume();
            until = System.currentTimeMillis() + 10_000;
            while (!flag("blockedGot") && System.currentTimeMillis() < until) {
                Thread.sleep(20);
            }
            System.out.println("  after the resume: blocked got it: " + flag("blockedGot"));
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
