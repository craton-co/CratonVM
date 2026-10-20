// Interpreter round i2 wave 54, lane L6: 1 000 `ThreadReference.suspend()` /
// `resume()` round trips of one running thread, with a field read between --
// the measurement scenario of
// docs/internal/fixed-bugs/interpreter-L6-proposal-debugger-park-and-settle-on-a-condvar-FIXED-20261018.md
// (`CRATONVM_DEBUG_PARK_CONDVAR`). It prints nothing timing-dependent; time
// the debugger side (`debug` below) on the host with the switch on and off,
// and against HotSpot. A resumed thread on the 1 ms poll runs again only at
// its next round; on the condvar, at once.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   ended
//
// Under a debugger:
//
//   javac -g -d out L6W54JdiSuspendResumeRoundTrips.java
//   cratonvm --java-home $JDK --jdwp-port 5801 -cp out L6W54JdiSuspendResumeRoundTrips wait
//   time java -cp out L6W54JdiSuspendResumeRoundTrips debug 5801 > transcript.txt
//
// HotSpot 25.0.3 as the debuggee (default flags, the Windows box):
//
//   == attach
//     rounds: 1000 stopped while suspended: 1000
//     ran between the rounds: true
//   == end
//     disconnected
//
// (The debugger side took about 1.2 s against HotSpot 25.0.3 on the Windows
// box, attach included.) `CRATONVM_FRAME_TRACE=1` prints one
// `[DEBUG_PARK] tid=<n> rounds=<n> notified=<n> condvar=<bool>` per park:
// `notified` > 0 only with the condvar (the positive control).
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;

public class L6W54JdiSuspendResumeRoundTrips {
    static final int ROUNDS = 1_000;
    /** Set by the debugger: 1 = start, 3 = done. */
    static volatile int phase;
    static volatile long counter;

    static void spin() {
        while (phase != 3) {
            counter++;
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
            phase = 3;
        }
        Thread spinner = new Thread(L6W54JdiSuspendResumeRoundTrips::spin, "spinner");
        spinner.setDaemon(true);
        spinner.start();
        while (debugged && phase != 3 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        spinner.join(10_000);
        System.out.println("ended");
    }

    static final class Debugger {
        static VirtualMachine vm;
        static ClassType type;

        static long counter() {
            return ((LongValue) type.getValue(type.fieldByName("counter"))).value();
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
            type = (ClassType) awaitClass("L6W54JdiSuspendResumeRoundTrips");
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(1));
            ThreadReference spinner = null;
            long until = System.currentTimeMillis() + 120_000;
            while (spinner == null) {
                for (ThreadReference t : vm.allThreads()) {
                    if (t.name().equals("spinner")) {
                        spinner = t;
                    }
                }
                if (System.currentTimeMillis() > until) {
                    fail("no spinner");
                }
                Thread.sleep(10);
            }
            while (counter() == 0) {
                Thread.sleep(1);
            }
            long first = counter();
            int stopped = 0;
            for (int i = 0; i < ROUNDS; i++) {
                spinner.suspend();
                long before = counter();
                if (spinner.isSuspended() && counter() == before) {
                    stopped++;
                }
                spinner.resume();
            }
            System.out.println("  rounds: " + ROUNDS + " stopped while suspended: " + stopped);
            // It runs between the rounds.
            until = System.currentTimeMillis() + 10_000;
            while (counter() == first && System.currentTimeMillis() < until) {
                Thread.sleep(1);
            }
            System.out.println("  ran between the rounds: " + (counter() != first));
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
