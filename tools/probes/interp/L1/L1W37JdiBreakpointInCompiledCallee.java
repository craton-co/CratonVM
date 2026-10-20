// Interpreter round i1 wave 37, lane L1: a JDWP breakpoint set in a method
// that a compiled caller calls, while that caller's loop runs compiled, for
// docs/internal/fixed-bugs/interpreter-L1-a-breakpoint-in-a-compiled-callee-of-a-running-compiled-caller-is-missed-FIXED-20261004.md.
//
// `loop()` calls `callee(i)` and then sleeps 2 ms, forever until the debugger
// sets `stop`. It is warmed first (2000 short calls) so that it and `callee`
// are compiled before the debugger attaches, and the long call is entered
// compiled. The debugger then sets a breakpoint at `callee`'s first location
// (SUSPEND_EVENT_THREAD) and prints whether the thread stopped there within
// 30 s, and in which method its caller frame is.
//
// Canonical transcript: no ids, no addresses, no timings.
// Run by the conformance runner, tools/jdi/run-jdi-conformance.sh
// (`--scenario L1W37JdiBreakpointInCompiledCallee`); in its default list since
// wave 38.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   sum>0=true
//
// Under a debugger (the debuggee waits up to five minutes):
//
//   javac -g -d out L1W37JdiBreakpointInCompiledCallee.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W37JdiBreakpointInCompiledCallee wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5791 -cp out L1W37JdiBreakpointInCompiledCallee wait)
//   java -cp out L1W37JdiBreakpointInCompiledCallee debug 5791 > transcript.txt     # HotSpot 25's java
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout; three runs, the
// same each time):
//
//   == attach
//   == breakpoint in the compiled callee
//     breakpoint hit: true
//     caller: loop
//   == end
//     disconnected
//
// CratonVM (from reading the code; the orchestrator's run is the
// measurement): in the JIT modes the breakpoint concerns `callee` only, so
// the doors refuse new entries into `callee`'s body and a loop-exit pause is
// asked, but `loop`'s compiled body is not concerned and keeps calling
// `callee`'s compiled body through its baked direct call (or runs it
// inlined): `breakpoint hit: false`. Under `--nojit` it matches HotSpot.
//
// Since wave 38 (lane L1) the breakpoint's class's dependents are withdrawn
// when it is set (`interpreter::note_breakpoint_classes_gained`,
// `JitRealm::withdraw_class_for_the_interpreter`): `loop`'s and `callee`'s
// bodies are evicted, made not entrant and have their exits forced, so the
// loop leaves at its next back edge and its next call is interpreted.
// Expected to match HotSpot in all four modes; `CRATONVM_DBG_JITC=1` prints
// `[cratonvm-jitc] breakpoint withdrawal: class=L1W37JdiBreakpointInCompiledCallee ...`.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W37JdiBreakpointInCompiledCallee {
    /** 0 while the loop may run; the debugger sets 1 to end it. */
    static volatile int stop;
    static volatile boolean debugged;
    static long sum;

    static int callee(int x) {
        return x * 3 + 1;
    }

    static long loop(boolean sleepy) throws InterruptedException {
        long s = 0;
        long until = System.currentTimeMillis() + 300_000;
        for (int i = 0; ; i++) {
            s += callee(i);
            if (!sleepy) {
                if (i == 500) {
                    return s;
                }
            } else {
                Thread.sleep(2);
                if (stop != 0 || (!debugged && i == 20)
                        || System.currentTimeMillis() > until) {
                    return s;
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
        for (int n = 0; n < 2000; n++) {
            sum += loop(false);
        }
        // Let a background compile of `loop` land before the call that stays.
        Thread.sleep(1000);
        sum += loop(true);
        System.out.println("sum>0=" + (sum > 0));
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
            type = awaitClass("L1W37JdiBreakpointInCompiledCallee");
            // The warm-up is over and the sleepy loop runs, compiled when the
            // JIT is on.
            Thread.sleep(4000);

            System.out.println("== breakpoint in the compiled callee");
            BreakpointRequest bp = erm.createBreakpointRequest(
                    type.methodsByName("callee").get(0).location());
            bp.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            bp.enable();
            ThreadReference hit = null;
            long until = System.currentTimeMillis() + 30_000;
            while (hit == null && System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(500);
                if (set == null) {
                    continue;
                }
                for (Event e : set) {
                    if (e instanceof BreakpointEvent b) {
                        hit = b.thread();
                    } else if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        fail("the debuggee ended early");
                    }
                }
                if (hit == null) {
                    set.resume();
                }
            }
            System.out.println("  breakpoint hit: " + (hit != null));
            if (hit != null) {
                System.out.println("  caller: " + hit.frame(1).location().method().name());
            }
            erm.deleteEventRequest(bp);

            System.out.println("== end");
            ((ClassType) type).setValue(type.fieldByName("stop"), vm.mirrorOf(1));
            if (hit != null) {
                hit.resume();
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
