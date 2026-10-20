// Interpreter round i1 wave 29, lane L1: a JDI scenario for item 2 of
// docs/internal/fixed-bugs/interpreter-L1-methods-served-without-a-frame-report-no-method-events-FIXED-20261004.md
// (virtual and interface calls an override could answer). Not in
// tools/jdi/run-jdi-conformance.sh's default list until a host run shows it
// matches; run it with `--scenario L1W29JdiVirtualStandInMethodEvents --modes
// "jdk-only jdk-only:nojit"` (like L1W27JdiStandInMethodEvents, --compatible
// keeps its stand-ins' dispatch).
//
//  * MethodEntry / MethodExit requests with SUSPEND_EVENT_THREAD, filtered to
//    the main thread and to String, StringBuilder, Integer and Long, while
//    `callThroughSupertypes`, whose every call site was linked before the
//    debugger attached, calls methods of those classes through a SUPERTYPE's
//    symbolic reference: `CharSequence.length` / `charAt` on a String and on
//    a StringBuilder (invokeinterface), `Number.intValue` / `longValue` on an
//    Integer / a Long and `Object.toString` on a StringBuilder
//    (invokevirtual). HotSpot reports each selected method at its first and
//    its returning bytecode index;
//  * a JDWP step into (STEP_LINE, STEP_INTO) from `stepHere`, which calls
//    `CharSequence.length` on a String: the thread stops in `String.length`.
//
// Printed are the events of Java (non-native) methods whose caller (frame 1)
// is the probe's `callThroughSupertypes`.
//
// Canonical transcript: no ids, no addresses, no timings.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   sum=1214
//
// Under a debugger (the debuggee waits up to five minutes for the debugger to
// set the static `attached`):
//
//   javac -g -d out L1W29JdiVirtualStandInMethodEvents.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W29JdiVirtualStandInMethodEvents wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5791 -cp out L1W29JdiVirtualStandInMethodEvents wait)
//   java -cp out L1W29JdiVirtualStandInMethodEvents debug 5791 > transcript.txt     # HotSpot 25's java
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout):
//
//   == attach
//   == stop at checkpoint 1
//   == method events through supertypes
//     entry java.lang.String.length()I index=0
//     exit java.lang.String.length()I index=10
//     entry java.lang.String.charAt(I)C index=0
//     exit java.lang.String.charAt(I)C index=15
//     entry java.lang.StringBuilder.length()I index=0
//     exit java.lang.StringBuilder.length()I index=4
//     entry java.lang.StringBuilder.charAt(I)C index=0
//     exit java.lang.StringBuilder.charAt(I)C index=5
//     entry java.lang.Integer.intValue()I index=0
//     exit java.lang.Integer.intValue()I index=4
//     entry java.lang.Long.longValue()J index=0
//     exit java.lang.Long.longValue()J index=4
//     entry java.lang.StringBuilder.toString()Ljava/lang/String; index=0
//     exit java.lang.StringBuilder.toString()Ljava/lang/String; index=19
//     entry java.lang.String.length()I index=0
//     exit java.lang.String.length()I index=10
//   == stop at checkpoint 2
//   == step into CharSequence.length
//     at stepHere index=0
//     step java.lang.String.length()I index=0 caller=stepHere
//   == stop at checkpoint 3
//   == end
//   vm death
//   disconnected
//
// CratonVM before wave 29 (from reading the code): a call answered by an
// intrinsic or a registered stand-in whose symbolic owner is a supertype
// (`CharSequence`, `Number`, `Object`) posts no entry or exit, and the step
// steps over `cs.length()`: the native-call funnel's hook
// (`jvmti_events::run_stood_in_java_method`) matched the callback against the
// symbolic owner's method only and ran only targets no override could replace.
// Wave 29: the hook selects the receiver's method (JVMS 5.4.6) and runs it
// when the callback stands in for exactly that method. Calls the interpreter
// already dispatches to bytecode report their events either way.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W29JdiVirtualStandInMethodEvents {
    static volatile boolean attached;
    static int counter;
    static final String text = "hello";
    static final StringBuilder builder = new StringBuilder("abc");
    static final Integer boxed = Integer.valueOf(700);
    static final Long wide = Long.valueOf(300L);
    static long sink;

    static void checkpoint(int count) {
        counter = count;
    }

    static long callThroughSupertypes(CharSequence s, CharSequence b, Number i, Number l, Object o) {
        long sum = 0;
        sum += s.length();
        sum += s.charAt(1);
        sum += b.length();
        sum += b.charAt(0);
        sum += i.intValue();
        sum += l.longValue();
        sum += o.toString().length();
        return sum;
    }

    static int stepHere(CharSequence cs) {
        return cs.length();
    }

    static long round() {
        return callThroughSupertypes(text, builder, boxed, wide, builder);
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        for (int i = 0; i < 3; i++) {
            sink += round();
            sink += stepHere(text);
        }
        checkpoint(0);
        boolean debugged = args.length == 1 && args[0].equals("wait");
        if (debugged) {
            waitForDebugger();
        }
        checkpoint(1);
        long sum = round();
        checkpoint(2);
        sum += stepHere(text);
        checkpoint(3);
        System.out.println("sum=" + sum);
    }

    static void waitForDebugger() throws InterruptedException {
        long until = System.currentTimeMillis() + 300_000;
        while (!attached) {
            if (System.currentTimeMillis() > until) {
                System.out.println("no debugger attached");
                System.exit(3);
            }
            Thread.sleep(20);
        }
    }

    static final class Debugger {
        static final String[] CLASSES = {
            "java.lang.String", "java.lang.StringBuilder", "java.lang.AbstractStringBuilder",
            "java.lang.Integer", "java.lang.Long",
        };
        static VirtualMachine vm;
        static ReferenceType type;

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
            EventRequestManager erm = vm.eventRequestManager();
            System.out.println("== attach");
            type = awaitClass("L1W29JdiVirtualStandInMethodEvents");
            BreakpointRequest bp = erm.createBreakpointRequest(
                    type.methodsByName("checkpoint").get(0).location());
            bp.setSuspendPolicy(EventRequest.SUSPEND_ALL);
            bp.enable();
            // The (no-op) resume first: sent after the flag, it could reach a
            // slow back end after the program had run into its first
            // SUSPEND_ALL stop, and release that stop.
            vm.resume();
            ((ClassType) type).setValue(type.fieldByName("attached"), vm.mirrorOf(true));
            ThreadReference main = awaitCheckpoint(1);

            System.out.println("== method events through supertypes");
            List<EventRequest> events = new ArrayList<>();
            for (String name : CLASSES) {
                MethodEntryRequest entry = erm.createMethodEntryRequest();
                entry.addThreadFilter(main);
                entry.addClassFilter(name);
                events.add(entry);
                MethodExitRequest exit = erm.createMethodExitRequest();
                exit.addThreadFilter(main);
                exit.addClassFilter(name);
                events.add(exit);
            }
            for (EventRequest r : events) {
                r.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
                r.enable();
            }
            vm.resume();
            main = awaitCheckpoint(2);
            for (EventRequest r : events) {
                erm.deleteEventRequest(r);
            }

            System.out.println("== step into CharSequence.length");
            BreakpointRequest atStep = erm.createBreakpointRequest(
                    type.methodsByName("stepHere").get(0).location());
            atStep.addThreadFilter(main);
            atStep.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            atStep.enable();
            vm.resume();
            ThreadReference stepper = awaitBreakpointIn("stepHere");
            erm.deleteEventRequest(atStep);
            StepRequest step = erm.createStepRequest(
                    stepper, StepRequest.STEP_LINE, StepRequest.STEP_INTO);
            step.addCountFilter(1);
            step.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            step.enable();
            stepper.resume();
            awaitStep();
            erm.deleteEventRequest(step);
            stepper.resume();
            awaitCheckpoint(3);
            System.out.println("== end");
            erm.deleteAllBreakpoints();
            vm.resume();
            awaitDeath();
        }

        /** Resume every event set until the debuggee's death and disconnection. */
        static void awaitDeath() throws Exception {
            while (true) {
                EventSet set;
                try {
                    set = vm.eventQueue().remove(60_000);
                } catch (VMDisconnectedException gone) {
                    System.out.println("disconnected");
                    return;
                }
                if (set == null) {
                    fail("the debuggee did not end");
                }
                for (Event e : set) {
                    if (e instanceof VMDeathEvent) {
                        System.out.println("vm death");
                    } else if (e instanceof VMDisconnectEvent) {
                        System.out.println("disconnected");
                        return;
                    }
                }
                set.resume();
            }
        }

        /** The thread stopped by a breakpoint in the probe's method `name`. */
        static ThreadReference awaitBreakpointIn(String name) throws Exception {
            long until = System.currentTimeMillis() + 120_000;
            while (System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(500);
                if (set == null) {
                    continue;
                }
                for (Event e : set) {
                    if (e instanceof BreakpointEvent b
                            && b.location().method().name().equals(name)) {
                        System.out.println("  at " + name + " index=" + b.location().codeIndex());
                        return b.thread();
                    }
                    if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        fail("the debuggee ended early");
                    }
                }
                set.resume();
            }
            fail("no breakpoint in " + name);
            return null;
        }

        /** Print the one step event and leave its thread suspended. */
        static void awaitStep() throws Exception {
            long until = System.currentTimeMillis() + 120_000;
            while (System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(500);
                if (set == null) {
                    continue;
                }
                for (Event e : set) {
                    if (e instanceof StepEvent s) {
                        Method m = s.location().method();
                        System.out.println("  step " + m.declaringType().name() + "." + m.name()
                                + m.signature() + " index=" + s.location().codeIndex()
                                + " caller=" + s.thread().frame(1).location().method().name());
                        return;
                    }
                    if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        fail("the debuggee ended early");
                    }
                }
                set.resume();
            }
            fail("no step event");
        }

        /** Print the events until checkpoint `n`'s breakpoint. */
        static ThreadReference awaitCheckpoint(int n) throws Exception {
            long until = System.currentTimeMillis() + 120_000;
            while (System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(500);
                if (set == null) {
                    continue;
                }
                ThreadReference stoppedAt = null;
                for (Event e : set) {
                    if (e instanceof MethodEntryEvent me) {
                        print("entry", me.method(), me.location(), me.thread());
                    } else if (e instanceof MethodExitEvent mx) {
                        print("exit", mx.method(), mx.location(), mx.thread());
                    } else if (e instanceof BreakpointEvent b) {
                        Method m = b.location().method();
                        if (m.declaringType().equals(type) && m.name().equals("checkpoint")) {
                            int count = ((IntegerValue) b.thread().frame(0)
                                    .getArgumentValues().get(0)).value();
                            System.out.println("== stop at checkpoint " + count);
                            if (count == n) {
                                stoppedAt = b.thread();
                            }
                        }
                    } else if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        fail("the debuggee ended early");
                    }
                }
                if (stoppedAt != null) {
                    return stoppedAt;
                }
                set.resume();
            }
            fail("no checkpoint " + n);
            return null;
        }

        static void print(String what, Method m, Location at, ThreadReference thread)
                throws Exception {
            if (m.isNative() || thread.frameCount() < 2) {
                return;
            }
            Method caller = thread.frame(1).location().method();
            if (!caller.declaringType().equals(type)
                    || !caller.name().equals("callThroughSupertypes")) {
                return;
            }
            System.out.println("  " + what + " " + m.declaringType().name() + "." + m.name()
                    + m.signature() + " index=" + at.codeIndex());
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
