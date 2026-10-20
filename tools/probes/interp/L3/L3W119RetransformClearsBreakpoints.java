// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 119, lane L3 (lane L9's review of
// `CRATONVM_REDEFINE_IDENTICAL_FAST_PATH`): what a JDWP breakpoint does when an
// agent retransforms its class with IDENTICAL bytes (no transformer). HotSpot's
// `VM_RedefineClasses::redefine_single_class` clears every JVMTI breakpoint in
// a class it redefines (`JvmtiBreakpoints::clearall_in_class_at_safepoint`),
// identical bytes included, and the JDWP back end is not told: the request
// stays registered and never fires again. CratonVM kept firing it (both
// redefinition paths); since wave 119 both clear it
// (`vm_exec::clear_breakpoints_of_redefined_class`,
// `CRATONVM_REDEFINE_CLEARS_BREAKPOINTS`, default on, `=0` off).
//
// The debuggee waits until the debugger has set a breakpoint at `Victim.v()`
// and set `armed`, then calls `v()`, retransforms `Victim`, calls `v()`,
// retransforms again, calls `v()`, and then asks for a NEW breakpoint
// (`rearmed`): the debugger sets one more request at the same location, and a
// `v()` call shows whether a request made after the redefinition fires. Last
// (`renewed`), the debugger deletes every request one by one and makes a third.
//
// SETUP: a jar `probe.jar` whose manifest has
//     Premain-Class: L3W119RetransformClearsBreakpoints$Agent
//     Can-Retransform-Classes: true
// containing L3W119RetransformClearsBreakpoints*.class (compiled with -g), then
//   debuggee, HotSpot:  java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5793 -javaagent:probe.jar -cp probe.jar L3W119RetransformClearsBreakpoints wait
//   debuggee, CratonVM: cratonvm --java-home $JDK --jdwp-port 5793 -javaagent:probe.jar -cp probe.jar L3W119RetransformClearsBreakpoints wait
//                       (a build with --features cratonvm-vm/experimental-debug; arms: with and
//                        without CRATONVM_REDEFINE_IDENTICAL_FAST_PATH=1, and
//                        CRATONVM_REDEFINE_CLEARS_BREAKPOINTS=0 for the old answer)
//   debugger (HotSpot 25's java): java -cp probe.jar L3W119RetransformClearsBreakpoints debug 5793
//
// Plain run (no agent, no debugger): prints "no agent".
//
// HotSpot 25.0.3 as the debuggee, the debugger's stdout (two runs, the same):
//     hits before retransform: 1
//     hits after two identical retransforms: 0
//     hits of a request made after them: 0
//     deleting the two earlier requests: [ok, InternalException: Unexpected JDWP Error: 41]
//     hits after deleting every request and making a new one: 1
// and the debuggee prints sum=35. The back end keeps ONE JVMTI breakpoint per
// location for all its requests there: the redefinition cleared it, a new
// request at the location does not set it again, and deleting the last
// request there asks JVMTI to clear a breakpoint that is gone (NOT_FOUND, 41).
// CratonVM before wave 119, read from the code (not run): 1, 2, 1, [ok, ok], 1.
import com.sun.jdi.Bootstrap;
import com.sun.jdi.ClassType;
import com.sun.jdi.Field;
import com.sun.jdi.Method;
import com.sun.jdi.ReferenceType;
import com.sun.jdi.VirtualMachine;
import com.sun.jdi.connect.AttachingConnector;
import com.sun.jdi.connect.Connector;
import com.sun.jdi.event.BreakpointEvent;
import com.sun.jdi.event.Event;
import com.sun.jdi.event.EventSet;
import com.sun.jdi.event.VMDeathEvent;
import com.sun.jdi.event.VMDisconnectEvent;
import com.sun.jdi.request.BreakpointRequest;
import java.lang.instrument.Instrumentation;
import java.util.Map;

public class L3W119RetransformClearsBreakpoints {
    static volatile Instrumentation inst;
    /** Set by the debugger once its first breakpoint stands. */
    static volatile boolean armed;
    /** Set by the debugger once its second request stands. */
    static volatile boolean rearmed;
    /** Set by the debugger once it deleted every request and made a third. */
    static volatile boolean renewed;
    /** The phase the debuggee is in, read by the debugger at each hit. */
    static volatile int phase;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Victim {
        public static int v() {
            return 7;
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length > 1 && args[0].equals("debug")) {
            debug(Integer.parseInt(args[1]));
            return;
        }
        Instrumentation i = inst;
        if (i == null) {
            System.out.println("no agent");
            return;
        }
        // Loaded and initialized up front, so the debugger finds it prepared.
        Class.forName(Victim.class.getName(), true, L3W119RetransformClearsBreakpoints.class.getClassLoader());
        long deadline = System.currentTimeMillis() + 300_000;
        while (!armed && System.currentTimeMillis() < deadline) {
            Thread.sleep(20);
        }
        phase = 1;
        int s = Victim.v();
        phase = 2;
        i.retransformClasses(Victim.class);
        s += Victim.v();
        i.retransformClasses(Victim.class);
        s += Victim.v();
        phase = 3;
        while (!rearmed && System.currentTimeMillis() < deadline) {
            Thread.sleep(20);
        }
        s += Victim.v();
        phase = 4;
        while (!renewed && System.currentTimeMillis() < deadline) {
            Thread.sleep(20);
        }
        s += Victim.v();
        System.out.println("sum=" + s);
    }

    static void debug(int port) throws Exception {
        AttachingConnector socket = Bootstrap.virtualMachineManager().attachingConnectors().stream()
                .filter(c -> c.transport().name().equals("dt_socket"))
                .findFirst()
                .orElseThrow();
        Map<String, Connector.Argument> cargs = socket.defaultArguments();
        cargs.get("port").setValue(Integer.toString(port));
        VirtualMachine vm = socket.attach(cargs);
        ReferenceType self = vm.classesByName(L3W119RetransformClearsBreakpoints.class.getName()).get(0);
        ReferenceType victim = null;
        while (victim == null) {
            var found = vm.classesByName(Victim.class.getName());
            if (found.isEmpty()) {
                Thread.sleep(20);
            } else {
                victim = found.get(0);
            }
        }
        setBreakpoint(vm, victim);
        setFlag(vm, self, "armed");
        int[] hits = new int[5];
        boolean done = false;
        boolean rearmedHere = false;
        boolean renewedHere = false;
        java.util.List<String> deletes = new java.util.ArrayList<>();
        while (!done) {
            EventSet set = vm.eventQueue().remove(50);
            if (set != null) {
                for (Event e : set) {
                    if (e instanceof BreakpointEvent) {
                        hits[Math.min(phase(self), 4)]++;
                    } else if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        done = true;
                    }
                }
            }
            if (!done && !rearmedHere && phase(self) == 3) {
                rearmedHere = true;
                setBreakpoint(vm, victim);
                setFlag(vm, self, "rearmed");
            }
            if (!done && !renewedHere && phase(self) == 4) {
                renewedHere = true;
                // Each request deleted on its own; what the delete answers is
                // part of the answer (HotSpot's back end asks JVMTI to clear a
                // breakpoint the redefinition already cleared).
                for (BreakpointRequest old : new java.util.ArrayList<>(
                        vm.eventRequestManager().breakpointRequests())) {
                    try {
                        vm.eventRequestManager().deleteEventRequest(old);
                        deletes.add("ok");
                    } catch (RuntimeException ex) {
                        deletes.add(ex.getClass().getSimpleName() + ": " + ex.getMessage());
                    }
                }
                setBreakpoint(vm, victim);
                setFlag(vm, self, "renewed");
            }
            if (!done && set != null) {
                set.resume();
            }
        }
        System.out.println("hits before retransform: " + hits[1]);
        System.out.println("hits after two identical retransforms: " + hits[2]);
        System.out.println("hits of a request made after them: " + hits[3]);
        System.out.println("deleting the two earlier requests: " + deletes);
        System.out.println("hits after deleting every request and making a new one: " + hits[4]);
    }

    static int phase(ReferenceType self) {
        try {
            return ((com.sun.jdi.IntegerValue) self.getValue(self.fieldByName("phase"))).value();
        } catch (RuntimeException e) {
            return 4;
        }
    }

    static void setBreakpoint(VirtualMachine vm, ReferenceType victim) {
        Method v = victim.methodsByName("v").get(0);
        BreakpointRequest bp = vm.eventRequestManager().createBreakpointRequest(v.location());
        bp.enable();
    }

    static void setFlag(VirtualMachine vm, ReferenceType self, String name) throws Exception {
        Field f = self.fieldByName(name);
        ((ClassType) self).setValue(f, vm.mirrorOf(true));
    }
}
