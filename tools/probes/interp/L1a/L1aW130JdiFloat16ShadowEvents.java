// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 130, lane L1a -- JDWP method events of
// `Float.float16ToFloat(S)F` and `Float.floatToFloat16(F)S`, the two Java
// methods with bytecode that HotSpot 25's template interpreter may serve from
// an intrinsic entry besides the `Math` ones, and which CratonVM registers as
// reviewed `NativeKind::Intrinsic` natives (`native-builtins/src/lang_math.rs`).
// The open question of the wave-126 section of
// docs/internal/fixed-bugs/interpreter-i40-L1-proposal-frame-only-method-events-for-reviewed-intrinsic-shadows-CLOSED-20261224.md
// (`CRATONVM_JDWP_INTRINSIC_SHADOW_METHOD_EVENTS`): does HotSpot post their
// entry and exit, as for `Class.getName`, or nothing, as for `Math.abs(D)D`?
// `Float.sum(FF)F`, plain bytecode no native shadows, is the control.
//
// The debugger asks for MethodEntry and MethodExit on `java.lang.Float`, on
// the main thread, suspending it, and counts per method the events whose
// caller is the probe's `ask` (three calls each). For each method with an
// event it prints the first entry's and the first exit's code index.
//
// Plain run (no debugger), stdout as HotSpot 25 (default flags and -Xint):
//   java -cp out L1aW130JdiFloat16ShadowEvents
// HOTSPOT_EXPECTED_BEGIN
// sum=47619.0
// HOTSPOT_EXPECTED_END
//
// Under a debugger (debug build; both modes; JIT and --nojit; the switch
// unset and =1):
//   javac -g -d out L1aW130JdiFloat16ShadowEvents.java
//   cratonvm [--compatible] --java-home $JDK --jdwp-port 5797 -cp out L1aW130JdiFloat16ShadowEvents wait &
//   java -cp out L1aW130JdiFloat16ShadowEvents debug 5797
//   (HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5797 -cp out L1aW130JdiFloat16ShadowEvents wait)
//   or: tools/jdi/run-jdi-conformance.sh --scenario L1aW130JdiFloat16ShadowEvents ...
//
// HotSpot 25.0.3 as the debuggee (run in wave 130, Windows x64, default flags and -Xint alike):
//   == attach
//     sum(FF)F entries=3 exits=3 entry-index=0 exit-index=3
//     entries from ask: 3 exits from ask: 3
//   == end
//     disconnected
//
// So HotSpot posts nothing for either conversion (only the control). Expected
// on CratonVM: the same with the switch unset (the funnel refuses to run
// their bodies, which read `$assertionsDisabled`); with
// `CRATONVM_JDWP_INTRINSIC_SHADOW_METHOD_EVENTS=1` the same since wave 130
// (`jvmti_events::hotspot_serves_from_a_float16_entry` in
// `interpreter::shadow_named_by_invoke`); before it, the frame-only arm would
// have added `float16ToFloat(S)F` and `floatToFloat16(F)S` rows.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1aW130JdiFloat16ShadowEvents {
    static volatile int go;
    static short half = (short) 0x3c00;
    static float single = 1.5f;

    static float ask() {
        float f = Float.float16ToFloat(half);
        short h = Float.floatToFloat16(single);
        return Float.sum(f, (float) h);
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        if (args.length == 1 && args[0].equals("wait")) {
            long until = System.currentTimeMillis() + 300_000;
            while (go == 0 && System.currentTimeMillis() < until) {
                Thread.sleep(10);
            }
        }
        float sum = 0;
        for (int i = 0; i < 3; i++) {
            sum += ask();
        }
        System.out.println("sum=" + sum);
    }

    static final class Debugger {
        static VirtualMachine vm;

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
            ReferenceType type = awaitClass("L1aW130JdiFloat16ShadowEvents");
            ThreadReference main = null;
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals("main")) {
                    main = t;
                }
            }
            EventRequestManager erm = vm.eventRequestManager();
            MethodEntryRequest entry = erm.createMethodEntryRequest();
            entry.addClassFilter("java.lang.Float");
            entry.addThreadFilter(main);
            entry.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            entry.enable();
            MethodExitRequest exit = erm.createMethodExitRequest();
            exit.addClassFilter("java.lang.Float");
            exit.addThreadFilter(main);
            exit.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            exit.enable();
            ((ClassType) type).setValue(type.fieldByName("go"), vm.mirrorOf(1));
            int entries = 0;
            int exits = 0;
            // method -> {entries, exits, first entry index, first exit index}
            TreeMap<String, long[]> per = new TreeMap<>();
            boolean ended = false;
            long until = System.currentTimeMillis() + 120_000;
            while (!ended && System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(5_000);
                if (set == null) {
                    break;
                }
                for (Event e : set) {
                    if (e instanceof LocatableEvent le && (e instanceof MethodEntryEvent
                            || e instanceof MethodExitEvent)) {
                        String caller;
                        try {
                            caller = le.thread().frame(1).location().method().name();
                        } catch (IncompatibleThreadStateException | IndexOutOfBoundsException x) {
                            caller = "?";
                        }
                        if (!caller.equals("ask")) {
                            continue;
                        }
                        Location l = le.location();
                        String m = l.method().name() + l.method().signature();
                        long[] row = per.computeIfAbsent(m, k -> new long[] {0, 0, -9, -9});
                        if (e instanceof MethodEntryEvent) {
                            entries++;
                            if (row[0]++ == 0) {
                                row[2] = l.codeIndex();
                            }
                        } else {
                            exits++;
                            if (row[1]++ == 0) {
                                row[3] = l.codeIndex();
                            }
                        }
                    } else if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        ended = true;
                    }
                }
                if (!ended) {
                    set.resume();
                }
            }
            for (Map.Entry<String, long[]> r : per.entrySet()) {
                long[] v = r.getValue();
                System.out.println("  " + r.getKey() + " entries=" + v[0] + " exits=" + v[1]
                        + " entry-index=" + v[2] + " exit-index=" + v[3]);
            }
            System.out.println("  entries from ask: " + entries + " exits from ask: " + exits);
            System.out.println("== end");
            if (!ended) {
                erm.deleteEventRequest(entry);
                erm.deleteEventRequest(exit);
                vm.resume();
            }
            while (!ended) {
                EventSet set;
                try {
                    set = vm.eventQueue().remove(60_000);
                } catch (VMDisconnectedException gone) {
                    break;
                }
                if (set == null) {
                    fail("the debuggee did not end");
                }
                for (Event e : set) {
                    if (e instanceof VMDisconnectEvent) {
                        ended = true;
                    }
                }
                if (!ended) {
                    set.resume();
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
