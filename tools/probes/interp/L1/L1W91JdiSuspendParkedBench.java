// Interpreter round i4 wave 91, lane L1 -- bench probe for the default-off
// `CRATONVM_PARK_LAZY_SUSPEND_AWARE` (round i3 wave 72, lane L6; follow-up (d)
// of docs/internal/fixed-bugs/interpreter-i71-L1-proposal-a-running-phase-before-a-blocking-park-DONE-20261224.md):
// a thread inside `CRATONVM_PARK_LAZY`'s running phase counts as "on its way"
// for `debug::await_suspension_settled`, so a `ThreadReference.suspend()` reply
// can wait out the lazy-park budget (2 ms; a timer tick or more on Windows).
// With the switch, a suspension ends the phase and the thread enters its
// blocked region at once.
//
// The debuggee's `parker` thread parks untimed in a loop; a `kicker` thread
// unparks it about every millisecond (busy-wait, so the parker spends most of
// its time inside the lazy phase). The debugger suspends and resumes `parker`
// ROUNDS times and times the round trips.
//
// Plain run (no debugger): stdout must equal HotSpot 25's: `ended`.
//
// Under a debugger (debug build, `experimental-debug`):
//
//   javac -g -d out L1W91JdiSuspendParkedBench.java
//   cratonvm [--compatible] --java-home $JDK --jdwp-port 5891 -cp out L1W91JdiSuspendParkedBench wait
//   java -cp out L1W91JdiSuspendParkedBench debug 5891
//   CRATONVM_PARK_LAZY_SUSPEND_AWARE=1 cratonvm ... (same arguments), then the debugger again
//   (and once with CRATONVM_PARK_LAZY_STW_WAKE=1 too; HotSpot as the debuggee:
//    java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5891 -cp out L1W91JdiSuspendParkedBench wait)
//
// Debugger stdout; the `ns/` line is the measurement (median ns per
// suspend+resume round trip), the rest is identical in every arm:
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3 debuggee, predicted)
// == attach
//   rounds: 200 suspended: 200
// == end
//   disconnected
// HOTSPOT_EXPECTED_END
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;
import java.util.concurrent.locks.LockSupport;

public class L1W91JdiSuspendParkedBench {
    static final int ROUNDS = 200;
    /** Set by the debugger: 1 = start, 3 = done. */
    static volatile int phase;
    static volatile long parks;

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
            phase = 3;
        }
        Thread parker = new Thread(() -> {
            while (phase != 3) {
                LockSupport.park();
                parks++;
            }
        }, "parker");
        parker.setDaemon(true);
        parker.start();
        Thread kicker = new Thread(() -> {
            while (phase != 3) {
                long t0 = System.nanoTime();
                while (System.nanoTime() - t0 < 1_000_000L) {
                    Thread.onSpinWait();
                }
                LockSupport.unpark(parker);
            }
            LockSupport.unpark(parker);
        }, "kicker");
        kicker.setDaemon(true);
        kicker.start();
        while (debugged && phase != 3 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        kicker.join(10_000);
        LockSupport.unpark(parker);
        parker.join(10_000);
        System.out.println("ended");
    }

    static final class Debugger {
        static VirtualMachine vm;
        static ClassType type;

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
            type = (ClassType) awaitClass("L1W91JdiSuspendParkedBench");
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(1));
            ThreadReference parker = null;
            long until = System.currentTimeMillis() + 120_000;
            while (parker == null) {
                for (ThreadReference t : vm.allThreads()) {
                    if (t.name().equals("parker")) {
                        parker = t;
                    }
                }
                if (System.currentTimeMillis() > until) {
                    fail("no parker");
                }
                Thread.sleep(10);
            }
            // Let the handoff run a while (the parker's park path warms up).
            while (((LongValue) type.getValue(type.fieldByName("parks"))).value() < 200) {
                if (System.currentTimeMillis() > until) {
                    fail("the parker never parked");
                }
                Thread.sleep(5);
            }
            long[] ns = new long[ROUNDS];
            int suspended = 0;
            for (int i = 0; i < ROUNDS; i++) {
                long t0 = System.nanoTime();
                parker.suspend();
                boolean s = parker.isSuspended();
                parker.resume();
                ns[i] = System.nanoTime() - t0;
                if (s) {
                    suspended++;
                }
                Thread.sleep(1);
            }
            Arrays.sort(ns);
            System.out.println("  rounds: " + ROUNDS + " suspended: " + suspended);
            System.out.println("  suspend-resume ns/round median " + ns[ROUNDS / 2] + " p90 " + ns[ROUNDS * 9 / 10]);
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
