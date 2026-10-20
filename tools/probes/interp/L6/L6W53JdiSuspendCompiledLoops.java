// Interpreter round i2 wave 53, lane L6: a JDWP suspension of a thread
// spinning in a compiled loop is complete at its reply, every time
// (docs/internal/fixed-bugs/interpreter-L6-jdwp-thread-suspend-is-occasionally-incomplete-at-its-reply-FIXED-20261017.md).
//
// The debuggee runs three `spinner` platform threads, each in a Java loop
// that bumps its own counter, long enough for the loops to run compiled. The
// debugger suspends them over and over -- 60 `ThreadReference.suspend()`s
// round the three, then 20 `VirtualMachine.suspend()`s -- and each time reads
// the suspended spinner's counter twice around a 20 ms pause, and its top
// frame. HotSpot's suspension is a handshake: the reads always agree.
// CratonVM's suspension only counted the thread and relied on one
// asynchronous loop-exit pause (at most three attempts) to send a compiled
// loop to its poll; a body entered just after that pause, or a peer the
// take-over froze in every attempt (a loaded host), ran on after the reply
// (1 run in 41 of `L1W49JdiSuspendCompletesBeforeReply`). The suspension's
// settle wait now asks the loop exits again while it waits (racy row: loop it
// on a loaded host; the positive control is the trace below).
//
// Canonical transcript: no ids, no addresses, no timings.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   ended
//
// Under a debugger:
//
//   javac -g -d out L6W53JdiSuspendCompiledLoops.java
//   cratonvm --java-home $JDK --jdwp-port 5794 -cp out L6W53JdiSuspendCompiledLoops wait
//   java -cp out L6W53JdiSuspendCompiledLoops debug 5794 > transcript.txt
//
// HotSpot 25.0.3 as the debuggee (default flags, the Windows box):
//
//   == attach
//     thread suspensions: 60 stable: true suspended: true top: spin
//     vm suspensions: 20 stable: true suspended: true top: spin
//   == end
//     disconnected
//
// Positive control: `CRATONVM_FRAME_TRACE=1` prints `[SUSPEND_SETTLED] ...
// nudges=<n>` with `n` above 0 whenever a suspension had to ask the loops
// again (a host under load; none on an idle one).
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;

public class L6W53JdiSuspendCompiledLoops {
    /** Set by the debugger: 1 = start, 3 = done. Set to 2 by the debuggee once every spinner ran. */
    static volatile int phase;
    static volatile long counter0;
    static volatile long counter1;
    static volatile long counter2;
    static volatile boolean stop;

    static void spin0() {
        while (!stop) {
            counter0++;
        }
    }

    static void spin1() {
        while (!stop) {
            counter1++;
        }
    }

    static void spin2() {
        while (!stop) {
            counter2++;
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
        Runnable[] bodies = {
            L6W53JdiSuspendCompiledLoops::spin0,
            L6W53JdiSuspendCompiledLoops::spin1,
            L6W53JdiSuspendCompiledLoops::spin2,
        };
        Thread[] spinners = new Thread[3];
        for (int i = 0; i < 3; i++) {
            spinners[i] = new Thread(bodies[i], "spinner" + i);
            spinners[i].setDaemon(true);
            spinners[i].start();
        }
        while (counter0 == 0 || counter1 == 0 || counter2 == 0) {
            Thread.sleep(1);
        }
        // Long enough for the loops to be compiled.
        Thread.sleep(500);
        if (debugged) {
            phase = 2;
        }
        while (debugged && phase != 3 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        stop = true;
        for (Thread t : spinners) {
            t.join(10_000);
        }
        System.out.println("ended");
    }

    static final class Debugger {
        static VirtualMachine vm;
        static ClassType type;

        static long counter(int i) {
            return ((LongValue) type.getValue(type.fieldByName("counter" + i))).value();
        }

        /** One check: spinner {@code i}'s counter holds still while it is suspended. */
        static boolean stable(int i) throws Exception {
            long first = counter(i);
            Thread.sleep(20);
            return first == counter(i);
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
            type = (ClassType) awaitClass("L6W53JdiSuspendCompiledLoops");
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(1));
            long until = System.currentTimeMillis() + 120_000;
            IntegerValue ready;
            do {
                if (System.currentTimeMillis() > until) {
                    fail("the spinners never ran");
                }
                Thread.sleep(20);
                ready = (IntegerValue) type.getValue(type.fieldByName("phase"));
            } while (ready.value() < 2);
            ThreadReference[] spinners = new ThreadReference[3];
            for (ThreadReference t : vm.allThreads()) {
                for (int i = 0; i < 3; i++) {
                    if (t.name().equals("spinner" + i)) {
                        spinners[i] = t;
                    }
                }
            }
            for (ThreadReference t : spinners) {
                if (t == null) {
                    fail("a spinner is missing");
                }
            }
            // 1. ThreadReference.Suspend, round the three spinners.
            long count = 0;
            boolean allStable = true;
            boolean allSuspended = true;
            Set<String> tops = new TreeSet<>();
            for (int round = 0; round < 60; round++) {
                int i = round % 3;
                ThreadReference t = spinners[i];
                t.suspend();
                count++;
                allStable &= stable(i);
                allSuspended &= t.isSuspended() && t.suspendCount() == 1;
                tops.add(t.frame(0).location().method().name().replaceAll("[0-9]+$", ""));
                t.resume();
                Thread.sleep(5);
            }
            System.out.println("  thread suspensions: " + count + " stable: " + allStable
                    + " suspended: " + allSuspended + " top: " + String.join(",", tops));
            // 2. VirtualMachine.Suspend.
            count = 0;
            allStable = true;
            allSuspended = true;
            tops.clear();
            for (int round = 0; round < 20; round++) {
                int i = round % 3;
                vm.suspend();
                count++;
                allStable &= stable(i);
                for (ThreadReference t : spinners) {
                    allSuspended &= t.isSuspended() && t.suspendCount() == 1;
                }
                tops.add(spinners[i].frame(0).location().method().name().replaceAll("[0-9]+$", ""));
                vm.resume();
                Thread.sleep(5);
            }
            System.out.println("  vm suspensions: " + count + " stable: " + allStable
                    + " suspended: " + allSuspended + " top: " + String.join(",", tops));
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
