// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 47, lane L2: a debugger's step reaches a compiled
// loop that is its class's OBSOLETE activation -- a loop that was already
// running when an agent redefined its class.
// docs/internal/fixed-bugs/interpreter-L2-proposal-interpreter-only-requests-let-obsolete-bodies-leave-through-their-source-FIXED-20261011.md
// docs/internal/fixed-bugs/i41-L1-compiled-bodies-without-an-exit-finish-their-activation-compiled-FIXED-20261007.md
// (shape 1 (b))
//
// `Tgt.spin(true)` runs a loop that sleeps 5 ms per iteration, so its thread
// spends nearly all its time in `Thread.sleep`. It is warmed first (3000
// short calls of `spin(false)`), so it is compiled before the long call
// enters it. While it runs, the debugger:
//
//  * asks the debuggee's agent thread to redefine `Tgt` with its own class
//    bytes (JVMTI RedefineClasses through java.lang.instrument; nothing moves
//    in the constant pool) and waits for it;
//  * suspends main, creates a LINE / INTO step filtered to `Tgt` (count 1,
//    SUSPEND_EVENT_THREAD), resumes main and waits for the step;
//  * then lets the loop end.
//
// A row prints `true` when its event arrived within 30 s.
//
// CratonVM before wave 47 (from reading the code; the orchestrator's run is
// the measurement): the redefinition spares the running loop's compiled body
// as its class's obsolete activation (`CompiledMethod::exits_spared_as_obsolete`),
// the step's interpreter-only withdrawal spares it again, and the safepoint
// verdict refuses every body compiled before its class's last redefinition
// (`jvmti_events::polling_body_must_leave`), so the loop finishes its
// activation compiled and `stepped in Tgt.spin` prints `false` in the JIT
// modes (`true` under `--nojit`). Since wave 47
// (`cratonvm_jit::not_entrant::INTERPRETER_ONLY_OBSOLETE_BODIES_LEAVE`) the
// withdrawal forces the body, the verdict hands its grant the bytecode it was
// compiled from, and a stash sink resumes the loop there, interpreted: `true`.
//
// Positive control on the host (debuggee side):
//   CRATONVM_DBG_JITC=1  ->  [cratonvm-jitc] obsolete method-entry body forced for an interpreter-only request (<interpreter-only>): L2W47JdiStepInObsoleteCompiledLoop$Tgt.spin...
//                            (or `obsolete OSR body forced ...` when the loop runs in an OSR body)
//   CRATONVM_DBG_DEOPT=1 ->  [cratonvm-deopt] obsolete method-entry body L2W47JdiStepInObsoleteCompiledLoop$Tgt.spin... under an interpreter-only request: its frame translates=true ...
//                            [cratonvm-deopt] withdrawn body told to leave: L2W47JdiStepInObsoleteCompiledLoop$Tgt.spin:(Z)J verdict=0x..
// None of these names `Tgt.spin` on the base (a451e608e).
//
// SETUP: a jar whose manifest has
//     Premain-Class: L2W47JdiStepInObsoleteCompiledLoop$Agent
//     Can-Redefine-Classes: true
// containing L2W47JdiStepInObsoleteCompiledLoop*.class (compiled with -g),
// then the debuggee and the debugger (HotSpot's JDI):
//
//   cratonvm --java-home $JDK [--compatible] [--nojit] --jdwp-port 5793 -javaagent:probe.jar -cp probe.jar L2W47JdiStepInObsoleteCompiledLoop wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5793 -javaagent:probe.jar -cp probe.jar L2W47JdiStepInObsoleteCompiledLoop wait)
//   java -cp probe.jar L2W47JdiStepInObsoleteCompiledLoop debug 5793 > transcript.txt
//
// The JDI conformance runner passes no -javaagent, so this probe is run by
// hand; without the agent the debuggee cannot redefine and the `redefined`
// row prints `false`.
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout; three runs, the
// same each time):
//
//   == attach
//   == redefine Tgt while its loop runs
//     redefined: true
//   == a step from the running loop
//     stepped in Tgt.spin: true
//   == end
//     disconnected
//
// and the debuggee prints `sum>0=true`.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;
import java.util.*;

public class L2W47JdiStepInObsoleteCompiledLoop {
    static volatile Instrumentation inst;
    /** 0 while the loop may run; the debugger sets 1 to end it. */
    static volatile int stop;
    /** The debugger sets 1 to ask for the redefinition. */
    static volatile int redefine;
    /** 1 once the agent thread redefined `Tgt`, -1 when it could not. */
    static volatile int redefined;
    static volatile boolean debugged;
    static long sum;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Tgt {
        static long spin(boolean sleepy) throws InterruptedException {
            long s = 0;
            long until = System.currentTimeMillis() + 300_000;
            for (int i = 0; ; i++) {
                s += i & 7;
                if (!sleepy) {
                    if (i == 2000) {
                        return s;
                    }
                } else {
                    Thread.sleep(5);
                    if (stop != 0 || (!debugged && i == 20)
                            || System.currentTimeMillis() > until) {
                        return s;
                    }
                }
            }
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        debugged = args.length == 1 && args[0].equals("wait");
        Thread swapper = new Thread(L2W47JdiStepInObsoleteCompiledLoop::swapWhenAsked, "swapper");
        swapper.setDaemon(true);
        swapper.start();
        for (int n = 0; n < 3000; n++) {
            sum += Tgt.spin(false);
        }
        // Let a background compile of `spin` land before the call that stays.
        Thread.sleep(1000);
        sum += Tgt.spin(true);
        System.out.println("sum>0=" + (sum > 0));
    }

    /** The agent thread: redefine `Tgt` with its own bytes once asked. */
    static void swapWhenAsked() {
        try {
            while (redefine == 0) {
                Thread.sleep(20);
            }
            if (inst == null) {
                redefined = -1;
                return;
            }
            byte[] bytes;
            try (InputStream in = Tgt.class.getResourceAsStream(
                    "L2W47JdiStepInObsoleteCompiledLoop$Tgt.class")) {
                bytes = in.readAllBytes();
            }
            inst.redefineClasses(new ClassDefinition(Tgt.class, bytes));
            redefined = 1;
        } catch (Throwable t) {
            System.out.println("redefinition failed: " + t);
            redefined = -1;
        }
    }

    static final class Debugger {
        static VirtualMachine vm;
        static ReferenceType type;
        static ThreadReference main;

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
            type = awaitClass("L2W47JdiStepInObsoleteCompiledLoop");
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals("main")) {
                    main = t;
                }
            }
            if (main == null) {
                fail("no main thread");
            }
            // The warm-up (3000 short loops and a 1 s sleep) is over, and the
            // sleepy loop runs, compiled when the JIT is on.
            Thread.sleep(5000);

            System.out.println("== redefine Tgt while its loop runs");
            ((ClassType) type).setValue(type.fieldByName("redefine"), vm.mirrorOf(1));
            boolean swapped = false;
            long until = System.currentTimeMillis() + 30_000;
            while (System.currentTimeMillis() < until) {
                int state = ((IntegerValue) type.getValue(type.fieldByName("redefined"))).value();
                if (state != 0) {
                    swapped = state == 1;
                    break;
                }
                Thread.sleep(50);
            }
            System.out.println("  redefined: " + swapped);
            // Let the loop run a few iterations past the redefinition.
            Thread.sleep(500);

            System.out.println("== a step from the running loop");
            main.suspend();
            StepRequest step = erm.createStepRequest(main, StepRequest.STEP_LINE,
                    StepRequest.STEP_INTO);
            step.addClassFilter("L2W47JdiStepInObsoleteCompiledLoop$Tgt");
            step.addCountFilter(1);
            step.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            step.enable();
            main.resume();
            String where = awaitStep();
            System.out.println("  stepped in Tgt.spin: " + "spin".equals(where));
            erm.deleteEventRequest(step);

            System.out.println("== end");
            ((ClassType) type).setValue(type.fieldByName("stop"), vm.mirrorOf(1));
            if (where != null) {
                main.resume();
            }
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

        /**
         * The name of the method the step's event stopped in, or null when
         * none arrived within 30 s. The thread stays suspended.
         */
        static String awaitStep() throws Exception {
            long until = System.currentTimeMillis() + 30_000;
            while (System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(500);
                if (set == null) {
                    continue;
                }
                for (Event e : set) {
                    if (e instanceof StepEvent se) {
                        return se.location().method().name();
                    } else if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        fail("the debuggee ended early");
                    }
                }
                set.resume();
            }
            return null;
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
