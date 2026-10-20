// Interpreter round i1 wave 49, lane L1: a JDWP suspension is complete when
// its reply arrives (stage 1 of
// docs/internal/fixed-bugs/interpreter-i48-L1-proposal-jdwp-suspension-completes-before-its-reply-DONE-20261224.md,
// `CRATONVM_DEBUG_SUSPEND_HANDSHAKE`).
//
// The debuggee runs `spinner`, a platform thread in a Java loop that bumps a
// static counter. The debugger suspends it three ways -- `ThreadReference.
// suspend()`, `VirtualMachine.suspend()`, and a `SUSPEND_ALL` breakpoint hit
// by another thread -- and each time, right after the reply (or the event),
// reads the counter, waits, and reads it again: HotSpot's suspension is a
// handshake, so the two reads agree and the thread's top frame is the loop.
// CratonVM counted the suspension and let the thread run on to its next
// suspend point, so a read right after the reply could see it still
// running (a racy row; the positive control is the trace below).
//
// Canonical transcript: no ids, no addresses, no timings.
// Run by the conformance runner, tools/jdi/run-jdi-conformance.sh
// (`--scenario L1W49JdiSuspendCompletesBeforeReply`).
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   ended
//
// Under a debugger:
//
//   javac -g -d out L1W49JdiSuspendCompletesBeforeReply.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W49JdiSuspendCompletesBeforeReply wait
//   java -cp out L1W49JdiSuspendCompletesBeforeReply debug 5791 > transcript.txt
//
// HotSpot 25.0.3 as the debuggee (default flags, the Windows box):
//
//   == attach
//     thread stable: true
//     thread suspended: true count: 1
//     thread top: spin
//     vm stable: true
//     vm suspended: true count: 1
//     vm top: spin
//     event stable: true
//     event suspended: true count: 1
//     event top: spin
//   == end
//     disconnected
//
// Positive control: `CRATONVM_FRAME_TRACE=1` prints `[SUSPEND_SETTLED]
// waited=<n> laggards=0 waited_us=<n>` with `waited` at least 1 for the
// spinner's suspensions (the switch off: no such line).
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W49JdiSuspendCompletesBeforeReply {
    /** Set by the debugger: 1 = start, 3 = done. Set to 2 by the debuggee once the spinner runs. */
    static volatile int phase;
    static volatile long counter;
    static volatile boolean stop;

    static void spin() {
        while (!stop) {
            counter++;
        }
    }

    static void hit() {
        // A breakpoint here stops the VM (SUSPEND_ALL).
        counter += 0;
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
        Thread spinner = new Thread(L1W49JdiSuspendCompletesBeforeReply::spin, "spinner");
        spinner.setDaemon(true);
        spinner.start();
        while (counter == 0) {
            Thread.sleep(1);
        }
        if (debugged) {
            phase = 2;
        }
        while (debugged && phase != 3 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
            if (phase == 4) {
                phase = 5;
                hit();
            }
        }
        stop = true;
        spinner.join(10_000);
        System.out.println("ended");
    }

    static final class Debugger {
        static VirtualMachine vm;
        static ClassType type;

        static long counter() {
            return ((LongValue) type.getValue(type.fieldByName("counter"))).value();
        }

        static void check(String how, ThreadReference spinner) throws Exception {
            long first = counter();
            Thread.sleep(100);
            long second = counter();
            System.out.println("  " + how + " stable: " + (first == second));
            System.out.println("  " + how + " suspended: " + spinner.isSuspended()
                    + " count: " + spinner.suspendCount());
            System.out.println("  " + how + " top: " + spinner.frame(0).location().method().name());
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
            type = (ClassType) awaitClass("L1W49JdiSuspendCompletesBeforeReply");
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(1));
            long until = System.currentTimeMillis() + 120_000;
            IntegerValue ready;
            do {
                if (System.currentTimeMillis() > until) {
                    fail("the spinner never ran");
                }
                Thread.sleep(20);
                ready = (IntegerValue) type.getValue(type.fieldByName("phase"));
            } while (ready.value() < 2);
            ThreadReference spinner = null;
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals("spinner")) {
                    spinner = t;
                }
            }
            if (spinner == null) {
                fail("no spinner thread");
            }
            // 1. ThreadReference.Suspend.
            spinner.suspend();
            check("thread", spinner);
            spinner.resume();
            Thread.sleep(50);
            // 2. VirtualMachine.Suspend.
            vm.suspend();
            check("vm", spinner);
            vm.resume();
            Thread.sleep(50);
            // 3. A SUSPEND_ALL event on another thread.
            Method hit = type.methodsByName("hit").get(0);
            BreakpointRequest bp = vm.eventRequestManager().createBreakpointRequest(hit.location());
            bp.setSuspendPolicy(EventRequest.SUSPEND_ALL);
            bp.enable();
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(4));
            EventSet set = null;
            boolean stopped = false;
            while (!stopped) {
                set = vm.eventQueue().remove(60_000);
                if (set == null) {
                    fail("no breakpoint event");
                }
                for (Event e : set) {
                    if (e instanceof BreakpointEvent) {
                        stopped = true;
                    }
                }
                if (!stopped) {
                    set.resume();
                }
            }
            check("event", spinner);
            bp.disable();
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(3));
            set.resume();
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
                    next.resume();
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
