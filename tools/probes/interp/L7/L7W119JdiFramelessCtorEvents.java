// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// probe (JDI; interpreter round i6 wave 119, lane L7)
//
// The JVMTI / JDWP MethodEntry and MethodExit row of
// docs/internal/proposals/interpreter/i24-L4-proposal-frameless-trivial-constructor-in-the-special-door-20260927.md:
// the special door answers a default constructor (`new Plain()`), a field-store constructor
// (`new Point(a, b)`, `new Span(long, long)`) and a constructor over a chain of default
// constructors (`new Leaf()`, Leaf extends Mid extends Base) WITHOUT pushing their frames
// (`invoke_fast::FIELD_CTOR_ELISION`, `CRATONVM_INTERP_TRIVIAL_CTOR_SUPER_CHAIN`,
// `CRATONVM_INTERP_FIELD_CTOR_WIDE_VALUES`). A debugger that asks for method events must still
// see every constructor's entry and exit, `Object.<init>` included, in call order and at the
// right depth: the elision is refused while `jvmti_requires_interpreter` holds. Every
// constructor runs 20 000 times BEFORE the debugger attaches, so each site is elided when the
// requests arrive; then `makeObjects` builds one of each under MethodEntry / MethodExit
// requests filtered to the main thread.
//
// Printed: the `<init>` events of the probe's classes and of `java.lang.Object`, with the frame
// depth. Canonical transcript: no ids, no addresses, no timings.
//
// Under a debugger (the debuggee waits up to five minutes for the debugger to set `attached`):
//
//   javac -g -d out L7W119JdiFramelessCtorEvents.java
//   cratonvm --java-home $JDK --jdwp-port 5793 -cp out L7W119JdiFramelessCtorEvents wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5793 -cp out L7W119JdiFramelessCtorEvents wait)
//   java -cp out L7W119JdiFramelessCtorEvents debug 5793 > transcript.txt     # HotSpot 25's java
//   Also with --nojit, and with CRATONVM_INTERP_TRIVIAL_CTOR_SUPER_CHAIN=0 /
//   CRATONVM_INTERP_FIELD_CTOR_WIDE_VALUES=0 (both default on): the transcript is the same.
//
// Plain run (no debugger), stdout:
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags; the plain run)
// sum=21 sink=400120000
// HOTSPOT_EXPECTED_END
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout):
//
//   == attach
//   == stop at checkpoint 1
//   == constructor events
//     entry L7W119JdiFramelessCtorEvents$Plain.<init>()V depth=3
//     entry java.lang.Object.<init>()V depth=4
//     exit java.lang.Object.<init>()V depth=4
//     exit L7W119JdiFramelessCtorEvents$Plain.<init>()V depth=3
//     entry L7W119JdiFramelessCtorEvents$Point.<init>(II)V depth=3
//     entry java.lang.Object.<init>()V depth=4
//     exit java.lang.Object.<init>()V depth=4
//     exit L7W119JdiFramelessCtorEvents$Point.<init>(II)V depth=3
//     entry L7W119JdiFramelessCtorEvents$Span.<init>(JJ)V depth=3
//     entry java.lang.Object.<init>()V depth=4
//     exit java.lang.Object.<init>()V depth=4
//     exit L7W119JdiFramelessCtorEvents$Span.<init>(JJ)V depth=3
//     entry L7W119JdiFramelessCtorEvents$Leaf.<init>()V depth=3
//     entry L7W119JdiFramelessCtorEvents$Mid.<init>()V depth=4
//     entry L7W119JdiFramelessCtorEvents$Base.<init>()V depth=5
//     entry java.lang.Object.<init>()V depth=6
//     exit java.lang.Object.<init>()V depth=6
//     exit L7W119JdiFramelessCtorEvents$Base.<init>()V depth=5
//     exit L7W119JdiFramelessCtorEvents$Mid.<init>()V depth=4
//     exit L7W119JdiFramelessCtorEvents$Leaf.<init>()V depth=3
//   == stop at checkpoint 2
//   == end
//   vm death
//   disconnected
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L7W119JdiFramelessCtorEvents {
    static volatile boolean attached;
    static int counter;
    static long sink;

    static final class Plain {
    }

    static final class Point {
        int x;
        int y;

        Point(int x, int y) {
            this.x = x;
            this.y = y;
        }
    }

    static final class Span {
        long start;
        long end;

        Span(long start, long end) {
            this.start = start;
            this.end = end;
        }
    }

    static class Base {
    }

    static class Mid extends Base {
    }

    static final class Leaf extends Mid {
    }

    static void checkpoint(int count) {
        counter = count;
    }

    static long makeObjects(int i) {
        long s = 0;
        s += new Plain() != null ? 1 : 0;
        Point p = new Point(i, 2);
        s += p.x + p.y;
        Span sp = new Span(i, 3L);
        s += sp.start + sp.end;
        s += new Leaf() != null ? 1 : 0;
        return s;
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        for (int i = 0; i < 20_000; i++) {
            sink += makeObjects(i);
        }
        checkpoint(0);
        boolean debugged = args.length == 1 && args[0].equals("wait");
        if (debugged) {
            waitForDebugger();
        }
        checkpoint(1);
        long sum = makeObjects(7);
        checkpoint(2);
        System.out.println("sum=" + sum + " sink=" + sink);
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
            type = awaitClass("L7W119JdiFramelessCtorEvents");
            BreakpointRequest bp = erm.createBreakpointRequest(
                    type.methodsByName("checkpoint").get(0).location());
            bp.setSuspendPolicy(EventRequest.SUSPEND_ALL);
            bp.enable();
            vm.resume();
            ((ClassType) type).setValue(type.fieldByName("attached"), vm.mirrorOf(true));
            ThreadReference main = awaitCheckpoint(1);

            System.out.println("== constructor events");
            List<EventRequest> events = new ArrayList<>();
            for (String name : new String[] {"L7W119JdiFramelessCtorEvents$*", "java.lang.Object"}) {
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
            awaitCheckpoint(2);
            for (EventRequest r : events) {
                erm.deleteEventRequest(r);
            }
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
                        int count = ((IntegerValue) b.thread().frame(0)
                                .getArgumentValues().get(0)).value();
                        System.out.println("== stop at checkpoint " + count);
                        if (count == n) {
                            stoppedAt = b.thread();
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
            String owner = m.declaringType().name();
            boolean ours = owner.startsWith("L7W119JdiFramelessCtorEvents$");
            boolean objectInit = owner.equals("java.lang.Object") && m.name().equals("<init>");
            if (!(ours && m.name().equals("<init>")) && !objectInit) {
                return;
            }
            System.out.println("  " + what + " " + owner + "." + m.name() + m.signature()
                    + " depth=" + thread.frameCount());
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
