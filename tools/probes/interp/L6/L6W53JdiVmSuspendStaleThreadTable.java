// Interpreter round i2 wave 53, lane L6: `VirtualMachine.suspend()` stops
// every thread that runs Java, whatever the server's thread table said when
// the debugger last looked at the threads.
//
// 1. `napper`, a platform thread in a Java loop that bumps a counter and,
//    when the debugger asks, sleeps 300 ms first. The debugger asks for a
//    nap, waits until `status()` reads SLEEPING (the server's thread table is
//    refreshed then), waits 400 ms more -- the napper woke and spins again --
//    and sends `VirtualMachine.suspend()`, which did not refresh the table.
//    CratonVM's settle wait skipped every thread whose cached status was not
//    RUNNING, so it took the woken napper for stopped and answered while it
//    still ran (`debug::suspension_on_its_way`). Twice, so a cold first look
//    and a warm second one are both covered.
// 2. `latecomer`, started by the debuggee after the debugger's last look at
//    the threads, spinning on its own counter: the settle wait checked only
//    the threads of the table, so it did not wait for it at all.
//
// HotSpot's suspension is a handshake: every counter holds still and every
// top frame is its loop.
//
// Canonical transcript: no ids, no addresses, no timings.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   ended
//
// Under a debugger:
//
//   javac -g -d out L6W53JdiVmSuspendStaleThreadTable.java
//   cratonvm --java-home $JDK --jdwp-port 5795 -cp out L6W53JdiVmSuspendStaleThreadTable wait
//   java -cp out L6W53JdiVmSuspendStaleThreadTable debug 5795 > transcript.txt
//
// HotSpot 25.0.3 as the debuggee (default flags, the Windows box):
//
//   == napper
//     napping: SLEEPING
//     vm stable: true
//     vm suspended: true count: 1
//     vm top: nap
//     napping: SLEEPING
//     vm stable: true
//     vm suspended: true count: 1
//     vm top: nap
//   == latecomer
//     vm stable: true
//     vm suspended: true count: 1
//     vm top: late
//   == end
//     disconnected
//
// Positive control: `CRATONVM_FRAME_TRACE=1` prints `[SUSPEND_SETTLED]
// waited=<n> ...` with `n` at least 1 for each `vm.suspend()` (before wave
// 53: no such line for the napper's, the napper was not waited for).
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;

public class L6W53JdiVmSuspendStaleThreadTable {
    /**
     * Set by the debugger: 1 = start, 4 = start the latecomer, 3 = done. Set
     * to 2 by the debuggee once the napper runs, to 5 once the latecomer does.
     */
    static volatile int phase;
    static volatile long counter;
    static volatile long lateCounter;
    static volatile boolean napRequested;
    static volatile boolean stop;

    static void nap() {
        while (!stop) {
            if (napRequested) {
                napRequested = false;
                try {
                    Thread.sleep(300);
                } catch (InterruptedException e) {
                    return;
                }
            }
            counter++;
        }
    }

    static void late() {
        while (!stop) {
            lateCounter++;
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
        Thread napper = new Thread(L6W53JdiVmSuspendStaleThreadTable::nap, "napper");
        napper.setDaemon(true);
        napper.start();
        while (counter == 0) {
            Thread.sleep(1);
        }
        if (debugged) {
            phase = 2;
        }
        Thread latecomer = null;
        while (debugged && phase != 3 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
            if (phase == 4 && latecomer == null) {
                latecomer = new Thread(L6W53JdiVmSuspendStaleThreadTable::late, "latecomer");
                latecomer.setDaemon(true);
                latecomer.start();
                while (lateCounter == 0) {
                    Thread.sleep(1);
                }
                phase = 5;
            }
        }
        stop = true;
        napper.join(10_000);
        if (latecomer != null) {
            latecomer.join(10_000);
        }
        System.out.println("ended");
    }

    static final class Debugger {
        static VirtualMachine vm;
        static ClassType type;

        static long read(String field) {
            return ((LongValue) type.getValue(type.fieldByName(field))).value();
        }

        static void awaitPhase(int wanted) throws Exception {
            long until = System.currentTimeMillis() + 120_000;
            while (((IntegerValue) type.getValue(type.fieldByName("phase"))).value() < wanted) {
                if (System.currentTimeMillis() > until) {
                    fail("the debuggee never reached phase " + wanted);
                }
                Thread.sleep(20);
            }
        }

        /** Suspend the VM; report whether {@code field} holds still. Leaves the VM suspended. */
        static void suspendAndRead(String field) throws Exception {
            vm.suspend();
            long first = read(field);
            Thread.sleep(100);
            long second = read(field);
            System.out.println("  vm stable: " + (first == second));
        }

        static void report(ThreadReference t) throws Exception {
            System.out.println("  vm suspended: " + t.isSuspended() + " count: " + t.suspendCount());
            System.out.println("  vm top: " + t.frame(0).location().method().name());
        }

        static ThreadReference named(String name) {
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals(name)) {
                    return t;
                }
            }
            fail("no thread " + name);
            return null;
        }

        static void napRound(ThreadReference napper) throws Exception {
            type.setValue(type.fieldByName("napRequested"), vm.mirrorOf(true));
            long until = System.currentTimeMillis() + 60_000;
            while (napper.status() != ThreadReference.THREAD_STATUS_SLEEPING) {
                if (System.currentTimeMillis() > until) {
                    fail("the napper never slept");
                }
                Thread.sleep(1);
            }
            System.out.println("  napping: SLEEPING");
            // It wakes after 300 ms and spins again; nothing refreshes the
            // server's thread table meanwhile.
            Thread.sleep(400);
            suspendAndRead("counter");
            report(napper);
            vm.resume();
            Thread.sleep(50);
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
            type = (ClassType) awaitClass("L6W53JdiVmSuspendStaleThreadTable");
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(1));
            awaitPhase(2);
            ThreadReference napper = named("napper");
            System.out.println("== napper");
            napRound(napper);
            napRound(napper);
            System.out.println("== latecomer");
            // The last look at the threads was above; the latecomer starts
            // after it, and only field reads happen until the suspension.
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(4));
            awaitPhase(5);
            Thread.sleep(200);
            suspendAndRead("lateCounter");
            report(named("latecomer"));
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(3));
            vm.resume();
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
