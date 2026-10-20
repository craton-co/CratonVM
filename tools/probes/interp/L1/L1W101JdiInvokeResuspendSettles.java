// Interpreter round i5 wave 101, lane L1: a JDWP invocation's closing
// re-suspension is complete when its reply arrives (the last suspension of
// docs/internal/fixed-bugs/interpreter-i48-L1-proposal-jdwp-suspension-completes-before-its-reply-DONE-20261224.md
// that did not wait; `CRATONVM_DEBUG_INVOKE_REPLY_SETTLE`, default ON).
//
// The debuggee runs `spinner`, a platform thread in a Java loop that bumps a
// static counter. A `SUSPEND_ALL` breakpoint stops `main` in `hit()`; the
// debugger then invokes the static `nop()` on `main`, once with options 0
// (every thread runs during the call, JDWP) and once with
// `INVOKE_SINGLE_THREADED` (the control: the spinner stays suspended). Right
// after each `invokeMethod` returns it reads the counter, waits 100 ms, and
// reads it again. HotSpot's `invoker_completeInvokeRequest` suspends every
// thread (`threadControl_suspendAll`, a JVMTI `SuspendThreadList`, which
// returns once each thread is stopped) before it sends the reply, so the two
// reads agree. CratonVM counted the spinner suspended again and replied, and
// the spinner ran on to its next suspend point after the reply (a racy row
// with the switch `=0`; the positive control is the trace below).
//
// Canonical transcript: no ids, no addresses, no timings.
// Run by the conformance runner, tools/jdi/run-jdi-conformance.sh
// (`--scenario L1W101JdiInvokeResuspendSettles`).
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   ended
//
// Under a debugger:
//
//   javac -g -d out L1W101JdiInvokeResuspendSettles.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W101JdiInvokeResuspendSettles wait
//   java -cp out L1W101JdiInvokeResuspendSettles debug 5791 > transcript.txt
//
// HotSpot 25 as the debuggee (derived from the back end's invoker.c; MEASURED in wave 116, JDK 25.0.3 Windows, same lines):
//
//   == attach
//     breakpoint stable: true
//     all-threads invoke: 7
//     all-threads stable: true
//     all-threads suspended: true count: 1
//     all-threads top: spin
//     single-threaded invoke: 7
//     single-threaded stable: true
//     single-threaded suspended: true count: 1
//     single-threaded top: spin
//   == end
//     disconnected
//
// Positive control: `CRATONVM_FRAME_TRACE=1` prints `[SUSPEND_SETTLED]
// waited=<n> ...` with `waited` at least 1 right after the all-threads
// invocation (with `CRATONVM_DEBUG_INVOKE_REPLY_SETTLE=0`: no such line there).
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W101JdiInvokeResuspendSettles {
    /** Set by the debugger: 1 = start, 4 = hit, 3 = done. 2 once the spinner runs. */
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

    static int nop() {
        return 7;
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
        Thread spinner = new Thread(L1W101JdiInvokeResuspendSettles::spin, "spinner");
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
            if (spinner != null) {
                System.out.println("  " + how + " suspended: " + spinner.isSuspended()
                        + " count: " + spinner.suspendCount());
                System.out.println("  " + how + " top: " + spinner.frame(0).location().method().name());
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
            type = (ClassType) awaitClass("L1W101JdiInvokeResuspendSettles");
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
            Method hit = type.methodsByName("hit").get(0);
            BreakpointRequest bp = vm.eventRequestManager().createBreakpointRequest(hit.location());
            bp.setSuspendPolicy(EventRequest.SUSPEND_ALL);
            bp.enable();
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(4));
            EventSet set = null;
            ThreadReference main = null;
            while (main == null) {
                set = vm.eventQueue().remove(60_000);
                if (set == null) {
                    fail("no breakpoint event");
                }
                for (Event e : set) {
                    if (e instanceof BreakpointEvent) {
                        main = ((BreakpointEvent) e).thread();
                    }
                }
                if (main == null) {
                    set.resume();
                }
            }
            bp.disable();
            check("breakpoint", null);
            Method nop = type.methodsByName("nop").get(0);
            Value seven = type.invokeMethod(main, nop, Collections.emptyList(), 0);
            System.out.println("  all-threads invoke: " + seven);
            check("all-threads", spinner);
            seven = type.invokeMethod(main, nop, Collections.emptyList(),
                    ClassType.INVOKE_SINGLE_THREADED);
            System.out.println("  single-threaded invoke: " + seven);
            check("single-threaded", spinner);
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
