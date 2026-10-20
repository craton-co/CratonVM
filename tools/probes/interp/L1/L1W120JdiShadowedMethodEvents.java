// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 120, lane L1 -- JDWP method events of a Java
// method a reviewed `Intrinsic` native shadows (the frame-only report of
// docs/internal/fixed-bugs/interpreter-i40-L1-proposal-frame-only-method-events-for-reviewed-intrinsic-shadows-CLOSED-20261224.md,
// `CRATONVM_JDWP_INTRINSIC_SHADOW_METHOD_EVENTS`, default off).
//
// `Class.getName()` has bytecode. CratonVM answers it with a reviewed
// `Intrinsic` registration whose bytecode it does not run
// (`jvmti_events::stood_in_bytecode_may_run` refuses it). HotSpot's
// interpreter reports its entry (at index 0) and its exit (at its
// `areturn`, with the value). The debugger asks for MethodEntry and
// MethodExit on `java.lang.Class`, on the main thread, suspending it, and
// counts the events whose caller is the probe's `ask` (three calls of
// `Target.class.getName()`). For each it prints the method, the location's
// code index, and (exit) the returned value.
//
// Plain run (no debugger), stdout as HotSpot 25: `names=3`.
//
// Under a debugger (debug build; both modes; JIT and --nojit; the switch
// unset and =1):
//   javac -g -d out L1W120JdiShadowedMethodEvents.java
//   cratonvm [--compatible] --java-home $JDK --jdwp-port 5796 -cp out L1W120JdiShadowedMethodEvents wait &
//   java -cp out L1W120JdiShadowedMethodEvents debug 5796
//   (HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5796 -cp out L1W120JdiShadowedMethodEvents wait)
//
// HotSpot 25.0.3 as the debuggee (run in wave 120, Windows):
//   == attach
//     entry getName()Ljava/lang/String; index=0 frame0=getName
//     exit getName()Ljava/lang/String; index-is-a-return=true value="L1W120JdiShadowedMethodEvents$Target"
//     entries from ask: 3 exits from ask: 3
//   == end
//     disconnected
//
// CratonVM (not run; this lane does not run cratonvm): with the switch unset
// the expected answer is no entry or exit from `ask` (the Intrinsic answers
// without a frame or a report), `entries from ask: 0 exits from ask: 0`.
// With the switch =1 and an interpreted `ask` the transcript should match
// HotSpot's; a JIT-compiled `ask` stays at 0 (the arm declines under a
// compiled caller).
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W120JdiShadowedMethodEvents {
    static final class Target {}

    static volatile int go;

    static String ask() {
        return Target.class.getName();
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        Class.forName(Target.class.getName());
        if (args.length == 1 && args[0].equals("wait")) {
            long until = System.currentTimeMillis() + 300_000;
            while (go == 0 && System.currentTimeMillis() < until) {
                Thread.sleep(10);
            }
        }
        int names = 0;
        for (int i = 0; i < 3; i++) {
            if (ask().endsWith("$Target")) {
                names++;
            }
        }
        System.out.println("names=" + names);
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
            ReferenceType type = awaitClass("L1W120JdiShadowedMethodEvents");
            ThreadReference main = null;
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals("main")) {
                    main = t;
                }
            }
            EventRequestManager erm = vm.eventRequestManager();
            MethodEntryRequest entry = erm.createMethodEntryRequest();
            entry.addClassFilter("java.lang.Class");
            entry.addThreadFilter(main);
            entry.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            entry.enable();
            MethodExitRequest exit = erm.createMethodExitRequest();
            exit.addClassFilter("java.lang.Class");
            exit.addThreadFilter(main);
            exit.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            exit.enable();
            ((ClassType) type).setValue(type.fieldByName("go"), vm.mirrorOf(1));
            int entries = 0;
            int exits = 0;
            List<String> lines = new ArrayList<>();
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
                        if (e instanceof MethodEntryEvent) {
                            entries++;
                            if (entries == 1) {
                                lines.add("  entry " + m + " index=" + l.codeIndex()
                                        + " frame0=" + le.thread().frame(0).location().method().name());
                            }
                        } else {
                            exits++;
                            if (exits == 1) {
                                Value v = ((MethodExitEvent) e).returnValue();
                                lines.add("  exit " + m + " index-is-a-return="
                                        + isReturn(l) + " value=" + v);
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
            for (String line : lines) {
                System.out.println(line);
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

        /** Is the location's code index the method's last return instruction's? */
        static boolean isReturn(Location l) {
            byte[] code = l.method().bytecodes();
            long index = l.codeIndex();
            if (index < 0 || index >= code.length) {
                return false;
            }
            int op = code[(int) index] & 0xff;
            return op >= 0xac && op <= 0xb1;
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
