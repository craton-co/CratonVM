// Interpreter round i1 wave 26, lane L1: the JDI conformance harness's
// `native method events` scenario, for
// docs/internal/fixed-bugs/interpreter-L1-jdwp-method-events-and-stop-miss-native-and-compiled-code-FIXED-20261005.md
// (section 2, native methods; section 3, VMDeath's suspend policy) and stage 1
// of the proposal interpreter-L1-proposal-native-method-events-at-the-native-funnel-FIXED-20260928.md:
//
//  * MethodEntry / MethodExit (with its return value) requests with
//    SUSPEND_NONE, filtered to the main thread, while the program calls four
//    genuine native methods from interpreted code — an instance native of a
//    final class (Runtime.availableProcessors) and three static ones
//    (Float.floatToRawIntBits, Float.intBitsToFloat,
//    Double.doubleToRawLongBits) — between two Java methods of its own: every
//    native's entry and exit is reported at location -1, in call order,
//    between the Java methods' events;
//  * the same requests with SUSPEND_EVENT_THREAD, filtered to
//    java.lang.Runtime: the thread is suspended at the native's entry and at
//    its exit, and its frames then list the native method first, at -1, above
//    its interpreted caller;
//  * a VMDeath request with SUSPEND_ALL (JDI creates one only when
//    CapabilitiesNew.canRequestVMDeathEvent is true): the event set suspends
//    the VM, which is held — the debugger reads a static — until the set is
//    resumed.
//
// Every call is made once before the debugger attaches, so each class is
// initialized and each call site linked when the events are asked for.
// Printed are the events whose method is the probe's own or native, or whose
// location is -1 (a Java method reported as a native would show).
//
// Canonical transcript: no ids, no addresses, no timings, no processor count.
// Run by the conformance runner, tools/jdi/run-jdi-conformance.sh
// (`--scenario L1W26JdiNativeMethodEvents`).
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   bits=1069547520 back=1.5 raw=4612811918334230528 processors>0=true
//
// Under a debugger (the debuggee waits up to five minutes for the debugger to
// set the static `attached`):
//
//   javac -g -d out L1W26JdiNativeMethodEvents.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W26JdiNativeMethodEvents wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5791 -cp out L1W26JdiNativeMethodEvents wait)
//   java -cp out L1W26JdiNativeMethodEvents debug 5791 > transcript.txt     # HotSpot 25's java
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout):
//
//   == attach
//     canRequestVMDeathEvent=true canGetMethodReturnValues=true
//   == stop at checkpoint 1
//   == events with SUSPEND_NONE
//     exit L1W26JdiNativeMethodEvents.checkpoint index=4 returns void
//     entry L1W26JdiNativeMethodEvents.readNatives index=0
//     entry java.lang.Runtime.availableProcessors native index=-1
//     exit java.lang.Runtime.availableProcessors native index=-1 returns positive=true
//     entry java.lang.Float.floatToRawIntBits native index=-1
//     exit java.lang.Float.floatToRawIntBits native index=-1 returns 1069547520
//     entry java.lang.Float.intBitsToFloat native index=-1
//     exit java.lang.Float.intBitsToFloat native index=-1 returns 1.5
//     entry java.lang.Double.doubleToRawLongBits native index=-1
//     exit java.lang.Double.doubleToRawLongBits native index=-1 returns 4612811918334230528
//     entry L1W26JdiNativeMethodEvents.twice index=0
//     exit L1W26JdiNativeMethodEvents.twice index=3 returns 2139095040
//     exit L1W26JdiNativeMethodEvents.readNatives index=41 returns 2139095040
//     entry L1W26JdiNativeMethodEvents.checkpoint index=0
//   == stop at checkpoint 2
//   == events with SUSPEND_EVENT_THREAD
//     entry java.lang.Runtime.availableProcessors native index=-1
//       suspended=true frameCount>=2=true
//       frame 0: availableProcessors index=-1 line=-1
//       frame 1: readProcessors index=- line=120
//     exit java.lang.Runtime.availableProcessors native index=-1 returns positive=true
//       suspended=true frameCount>=2=true
//       frame 0: availableProcessors index=-1 line=-1
//       frame 1: readProcessors index=- line=120
//   == stop at checkpoint 3
//   == vm death
//     vm death policy=all
//     held: counter=3
//   disconnected
//
// CratonVM before wave 26 (from reading the code; the orchestrator's run is
// the measurement): the first section listed only the probe's own methods
// (no native pushes an interpreter frame, and the interpreter's suspend point
// was the only producer of method events); the second section saw no event
// and the program ran to checkpoint 3; and `createVMDeathRequest` threw
// UnsupportedOperationException (canRequestVMDeathEvent was false).
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W26JdiNativeMethodEvents {
    static volatile boolean attached;
    static Runtime runtime;
    static int bits;
    static float back;
    static long raw;
    static int processors;
    static int counter;

    static void checkpoint(int count) {
        counter = count;
    }

    static int readNatives() {
        processors = runtime.availableProcessors();
        bits = Float.floatToRawIntBits(1.5f);
        back = Float.intBitsToFloat(bits);
        raw = Double.doubleToRawLongBits(2.5);
        return twice(bits);
    }

    static int twice(int x) {
        return x * 2;
    }

    static int readProcessors() {
        return runtime.availableProcessors();
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        runtime = Runtime.getRuntime();
        readNatives();
        readProcessors();
        checkpoint(0);
        boolean debugged = args.length == 1 && args[0].equals("wait");
        if (debugged) {
            waitForDebugger();
        }
        checkpoint(1);
        readNatives();
        checkpoint(2);
        int again = readProcessors();
        checkpoint(3);
        System.out.println("bits=" + bits + " back=" + back + " raw=" + raw
                + " processors>0=" + (processors > 0 && again > 0));
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
            System.out.println("  canRequestVMDeathEvent=" + vm.canRequestVMDeathEvent()
                    + " canGetMethodReturnValues=" + vm.canGetMethodReturnValues());
            type = awaitClass("L1W26JdiNativeMethodEvents");
            BreakpointRequest bp = erm.createBreakpointRequest(
                    type.methodsByName("checkpoint").get(0).location());
            bp.setSuspendPolicy(EventRequest.SUSPEND_ALL);
            bp.enable();
            if (vm.canRequestVMDeathEvent()) {
                VMDeathRequest death = erm.createVMDeathRequest();
                death.setSuspendPolicy(EventRequest.SUSPEND_ALL);
                death.enable();
            }
            // The (no-op) resume first: sent after the flag, it could reach a
            // slow back end after the program had run into its first
            // SUSPEND_ALL stop, and release that stop.
            vm.resume();
            ((ClassType) type).setValue(type.fieldByName("attached"), vm.mirrorOf(true));
            ThreadReference main = awaitCheckpoint(1, null);

            System.out.println("== events with SUSPEND_NONE");
            List<EventRequest> quiet = new ArrayList<>();
            MethodEntryRequest entries = erm.createMethodEntryRequest();
            entries.addThreadFilter(main);
            quiet.add(entries);
            MethodExitRequest exits = erm.createMethodExitRequest();
            exits.addThreadFilter(main);
            quiet.add(exits);
            for (EventRequest r : quiet) {
                r.setSuspendPolicy(EventRequest.SUSPEND_NONE);
                r.enable();
            }
            vm.resume();
            awaitCheckpoint(2, null);
            for (EventRequest r : quiet) {
                erm.deleteEventRequest(r);
            }

            System.out.println("== events with SUSPEND_EVENT_THREAD");
            List<EventRequest> stopping = new ArrayList<>();
            MethodEntryRequest entry = erm.createMethodEntryRequest();
            entry.addThreadFilter(main);
            entry.addClassFilter("java.lang.Runtime");
            stopping.add(entry);
            MethodExitRequest exit = erm.createMethodExitRequest();
            exit.addThreadFilter(main);
            exit.addClassFilter("java.lang.Runtime");
            stopping.add(exit);
            for (EventRequest r : stopping) {
                r.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
                r.enable();
            }
            vm.resume();
            awaitCheckpoint(3, stopping);
            for (EventRequest r : stopping) {
                erm.deleteEventRequest(r);
            }

            System.out.println("== vm death");
            erm.deleteAllBreakpoints();
            vm.resume();
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
                boolean death = false;
                for (Event e : set) {
                    if (e instanceof VMDeathEvent) {
                        death = true;
                    } else if (e instanceof VMDisconnectEvent) {
                        System.out.println("disconnected");
                        return;
                    }
                }
                if (death) {
                    System.out.println("  vm death policy=" + policy(set.suspendPolicy()));
                    if (set.suspendPolicy() == EventRequest.SUSPEND_ALL) {
                        // The VM is held: it answers.
                        Value v = type.getValue(type.fieldByName("counter"));
                        System.out.println("  held: counter=" + v);
                    }
                }
                set.resume();
            }
        }

        static String policy(int p) {
            switch (p) {
                case EventRequest.SUSPEND_ALL: return "all";
                case EventRequest.SUSPEND_EVENT_THREAD: return "thread";
                default: return "none";
            }
        }

        /** Print the method events until checkpoint `n`'s breakpoint. */
        static ThreadReference awaitCheckpoint(int n, List<EventRequest> stopping)
                throws Exception {
            long until = System.currentTimeMillis() + 120_000;
            while (System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(500);
                if (set == null) {
                    continue;
                }
                ThreadReference stoppedAt = null;
                for (Event e : set) {
                    if (e instanceof MethodEntryEvent me) {
                        print("entry", me.method(), me.location(), null, me.thread(),
                                stopping != null);
                    } else if (e instanceof MethodExitEvent mx) {
                        print("exit", mx.method(), mx.location(), mx.returnValue(), mx.thread(),
                                stopping != null);
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

        static void print(String what, Method m, Location at, Value returned,
                ThreadReference thread, boolean frames) throws Exception {
            boolean own = m.declaringType().equals(type);
            if (!own && !m.isNative() && at.codeIndex() != -1) {
                return;
            }
            StringBuilder line = new StringBuilder("  ").append(what).append(' ')
                    .append(m.declaringType().name()).append('.').append(m.name())
                    .append(m.isNative() ? " native" : "")
                    .append(" index=").append(at.codeIndex());
            if (returned != null) {
                line.append(" returns ").append(describe(m, returned));
            }
            System.out.println(line);
            if (frames) {
                System.out.println("    suspended=" + thread.isSuspended()
                        + " frameCount>=2=" + (thread.frameCount() >= 2));
                for (int i = 0; i < 2; i++) {
                    Location l = thread.frame(i).location();
                    System.out.println("    frame " + i + ": " + l.method().name()
                            + " index=" + (l.method().isNative() ? l.codeIndex() : "-")
                            + " line=" + l.lineNumber());
                }
            }
        }

        static String describe(Method m, Value v) {
            if (v instanceof VoidValue) {
                return "void";
            }
            if (m.name().equals("availableProcessors") || m.name().equals("readProcessors")) {
                return "positive=" + (((IntegerValue) v).value() > 0);
            }
            return String.valueOf(v);
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
