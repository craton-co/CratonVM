// Interpreter round i2 wave 62, lane L7: JDWP observers armed while a
// call-free loop runs are seen by that loop -- the JDWP half of the control
// for `CRATONVM_INTERP_HOISTED_OBSERVERS` (proposal
// i61-L7-proposal-the-loop-hoists-the-observer-flags-and-the-poll-word-refreshes-them-20261025.md;
// the C JVMTI half is L7W62ObserverArmedMidLoop). With the switch on, the
// dispatch loop reads the field-watch union and the JDWP gate only at its
// loop-top slow path, which `debug::publish_debugger_gates` (every JDWP
// command) sends it to; the loops below have no call and no back-edge refresh
// to fall back on. A conformance check, not a positive control: measured with
// the bumps disabled by a temporary edit, every row still answers, because a
// JDWP command reaches a call-free loop another way too (a pause, or a
// field-site miss after the server's class work moves a site epoch, whose
// slow handler reads the watch union itself). The discriminating control is
// L7W62ObserverArmedMidLoop's `step` / `self` rows.
//
// The debuggee's `main` runs three loops, each until the debugger ends it
// (or, undebugged, for 1000 iterations): `watchLoop` reads `Box.v`,
// `breakLoop` adds a counter, `entryLoop` calls `tick`. The debugger, each
// time the loop has run two seconds, arms an access watch on `Box.v`, then
// breakpoints on every line of `breakLoop` (and a line step from the hit),
// then a method entry request, each filtered to the main thread
// (SUSPEND_EVENT_THREAD), and prints whether the event came within 30 s and
// in which method.
//
// Canonical transcript: no ids, no addresses, no timings.
// Run by the conformance runner, tools/jdi/run-jdi-conformance.sh
// (`--scenario L7W62JdiObserverArmedMidLoop`), with the switch on and off:
// `CRATONVM_INTERP_HOISTED_OBSERVERS=1` / `=0` in the runner's environment.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   sum>0=true
//
// Under a debugger (the debuggee waits up to five minutes per loop):
//
//   javac -g -d out L7W62JdiObserverArmedMidLoop.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L7W62JdiObserverArmedMidLoop wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5791 -cp out L7W62JdiObserverArmedMidLoop wait)
//   java -cp out L7W62JdiObserverArmedMidLoop debug 5791 > transcript.txt     # HotSpot 25's java
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout; three runs, the
// same each time):
//
//   == attach
//   == access watch on a field a call-free loop reads
//     access hit: true
//     in: watchLoop
//   == breakpoints in a call-free loop
//     breakpoint hit: true
//     in: breakLoop
//   == a line step from the breakpoint
//     step hit: true
//     in: breakLoop
//   == method entry from a running loop
//     entry hit: true
//     in: tick
//   == end
//     disconnected
//
// CratonVM (wave 62, a debug `cratonvm` with `experimental-debug`, jdk-only,
// with and without `--nojit`, the switch on and off): the same.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L7W62JdiObserverArmedMidLoop {
    /** The loops end at 1, 2 and 3; the debugger raises it. */
    static volatile int stop;
    static volatile boolean debugged;
    static long sum;

    static final class Box {
        int v = 7;
    }

    static long watchLoop(Box b) {
        long s = 0;
        int i = 0;
        while (stop < 1) {
            s += b.v;
            i++;
            if (!debugged && i == 1000) {
                break;
            }
        }
        return s;
    }

    static long breakLoop() {
        long s = 0;
        int i = 0;
        while (stop < 2) {
            s += i;
            i++;
            if (!debugged && i == 1000) {
                break;
            }
        }
        return s;
    }

    static int tick(int x) {
        return x + 1;
    }

    static long entryLoop() {
        int s = 0;
        int i = 0;
        while (stop < 3) {
            s = tick(s);
            i++;
            if (!debugged && i == 1000) {
                break;
            }
        }
        return s;
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        debugged = args.length == 1 && args[0].equals("wait");
        Box box = new Box();
        sum += watchLoop(box);
        sum += breakLoop();
        sum += entryLoop();
        System.out.println("sum>0=" + (sum > 0));
    }

    static final class Debugger {
        static VirtualMachine vm;
        static EventRequestManager erm;
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
            erm = vm.eventRequestManager();
            System.out.println("== attach");
            type = (ClassType) awaitClass("L7W62JdiObserverArmedMidLoop");
            ReferenceType box = awaitClass("L7W62JdiObserverArmedMidLoop$Box");
            ThreadReference main = null;
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals("main")) {
                    main = t;
                }
            }
            if (main == null) {
                fail("no main thread");
            }

            System.out.println("== access watch on a field a call-free loop reads");
            Thread.sleep(2000);
            AccessWatchpointRequest watch = erm.createAccessWatchpointRequest(box.fieldByName("v"));
            watch.addThreadFilter(main);
            watch.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            watch.enable();
            LocatableEvent hit = await(AccessWatchpointEvent.class);
            erm.deleteEventRequest(watch);
            report("access", hit);
            end(1, hit);

            System.out.println("== breakpoints in a call-free loop");
            Thread.sleep(2000);
            Method breakLoop = type.methodsByName("breakLoop").get(0);
            List<BreakpointRequest> bps = new ArrayList<>();
            for (Location at : breakLoop.allLineLocations()) {
                BreakpointRequest bp = erm.createBreakpointRequest(at);
                bp.addThreadFilter(main);
                bp.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
                bp.enable();
                bps.add(bp);
            }
            hit = await(BreakpointEvent.class);
            erm.deleteEventRequests(bps);
            report("breakpoint", hit);

            System.out.println("== a line step from the breakpoint");
            if (hit != null) {
                StepRequest step = erm.createStepRequest(main, StepRequest.STEP_LINE, StepRequest.STEP_OVER);
                step.addCountFilter(1);
                step.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
                step.enable();
                hit.thread().resume();
                hit = await(StepEvent.class);
                erm.deleteEventRequest(step);
            }
            report("step", hit);
            end(2, hit);

            System.out.println("== method entry from a running loop");
            Thread.sleep(2000);
            MethodEntryRequest entry = erm.createMethodEntryRequest();
            entry.addClassFilter(type);
            entry.addThreadFilter(main);
            entry.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            entry.enable();
            hit = await(MethodEntryEvent.class);
            erm.deleteEventRequest(entry);
            report("entry", hit);
            end(3, hit);

            System.out.println("== end");
            while (true) {
                EventSet set;
                try {
                    set = vm.eventQueue().remove(60_000);
                } catch (VMDisconnectedException gone) {
                    System.out.println("  disconnected");
                    return;
                }
                if (set == null) {
                    fail("the debuggee did not end");
                }
                for (Event e : set) {
                    if (e instanceof VMDisconnectEvent) {
                        System.out.println("  disconnected");
                        return;
                    }
                }
                set.resume();
            }
        }

        /** The first event of `kind` within 30 s (its thread left suspended), or null. */
        static LocatableEvent await(Class<? extends LocatableEvent> kind) throws Exception {
            long until = System.currentTimeMillis() + 30_000;
            while (System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(500);
                if (set == null) {
                    continue;
                }
                LocatableEvent found = null;
                for (Event e : set) {
                    if (kind.isInstance(e)) {
                        found = (LocatableEvent) e;
                    } else if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        fail("the debuggee ended early");
                    }
                }
                if (found != null) {
                    return found;
                }
                set.resume();
            }
            return null;
        }

        static void report(String what, LocatableEvent hit) {
            System.out.println("  " + what + " hit: " + (hit != null));
            if (hit != null) {
                System.out.println("  in: " + hit.location().method().name());
            }
        }

        /** End the running loop and let the thread go. */
        static void end(int phase, LocatableEvent hit) throws Exception {
            type.setValue(type.fieldByName("stop"), vm.mirrorOf(phase));
            if (hit != null) {
                hit.thread().resume();
            }
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
