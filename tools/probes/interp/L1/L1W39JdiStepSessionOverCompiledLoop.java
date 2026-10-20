// Interpreter round i1 wave 39, lane L1: fifty JDWP steps through a warmed
// loop, the positive control of the stepping-session hold for
// docs/internal/fixed-bugs/interpreter-L1-each-jdwp-step-withdraws-every-compiled-body-FIXED-20261003.md
// (each step request used to withdraw every compiled body of the VM, and the
// hot set was compiled again before the next step flushed it again).
//
// `loop(true)` calls `Helper.work(i)` and sleeps 2 ms per iteration; it is
// warmed first (2000 short calls of `loop(false)`) so that `loop` and
// `Helper.work` are compiled before the debugger attaches. The debugger sets
// a breakpoint on the `work` line of `loop` (main thread,
// SUSPEND_EVENT_THREAD), deletes it once hit, and then steps 50 times: each
// step a LINE / OVER request created, the thread resumed, the step's event
// awaited, and the request deleted, as an IDE's "step over" does. It prints
// how many steps completed, whether every one stopped in `loop`, and the
// distinct lines they stopped at, relative to the breakpoint's line.
//
// Canonical transcript: no ids, no addresses, no timings.
// Run by the conformance runner, tools/jdi/run-jdi-conformance.sh
// (`--scenario L1W39JdiStepSessionOverCompiledLoop`).
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   sum>0=true
//
// Under a debugger (the debuggee waits up to five minutes):
//
//   javac -g -d out L1W39JdiStepSessionOverCompiledLoop.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W39JdiStepSessionOverCompiledLoop wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5791 -cp out L1W39JdiStepSessionOverCompiledLoop wait)
//   java -cp out L1W39JdiStepSessionOverCompiledLoop debug 5791 > transcript.txt     # HotSpot 25's java
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout; three runs, the
// same each time):
//
//   == attach
//   == breakpoint in the compiled loop
//     breakpoint hit: true
//     in: loop
//   == fifty steps over
//     steps completed: 50
//     every step in loop: true
//     lines from the breakpoint: [-1, 0, 1, 6, 7, 8]
//   == end
//     disconnected
//
// The positive control (CratonVM, JIT modes): run the debuggee with
// `CRATONVM_DBG_JITC=1`; its log (the runner's
// `<scenario>.<tag>.debuggee.txt`) must hold exactly ONE
// `[cratonvm-jitc] interpreter-only withdrawal: source=jdwp` line (the first
// step) and 49 `[cratonvm-jitc] interpreter-only withdrawal held:
// source=jdwp` lines (the other steps). Before wave 39 it held 50
// `interpreter-only withdrawal:` lines, one flush per step. The breakpoint
// prints its own `breakpoint withdrawal:` line (scoped to this class).
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W39JdiStepSessionOverCompiledLoop {
    /** 0 while the loop may run; the debugger sets 1 to end it. */
    static volatile int stop;
    static volatile boolean debugged;
    static long sum;

    static final class Helper {
        static int work(int x) {
            return x * 31 + 7;
        }
    }

    static long loop(boolean sleepy) throws InterruptedException {
        long s = 0;
        long until = System.currentTimeMillis() + 300_000;
        for (int i = 0; ; i++) {
            s += Helper.work(i); // BREAKPOINT_LINE
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
            ReferenceType type = awaitClass("L1W39JdiStepSessionOverCompiledLoop");
            // The warm-up is over and the sleepy loop runs, compiled when the
            // JIT is on.
            Thread.sleep(4000);
            ThreadReference main = null;
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals("main")) {
                    main = t;
                }
            }
            if (main == null) {
                fail("no main thread");
            }
            Method loop = type.methodsByName("loop").get(0);
            // The line of `s += Helper.work(i);`: the fourth distinct line of
            // `loop` (after `long s`, `long until` and the `for` header).
            TreeSet<Integer> lines = new TreeSet<>();
            for (Location l : loop.allLineLocations()) {
                lines.add(l.lineNumber());
            }
            int bpLine = new ArrayList<>(lines).get(3);
            Location bpAt = loop.locationsOfLine(bpLine).get(0);

            System.out.println("== breakpoint in the compiled loop");
            BreakpointRequest bp = erm.createBreakpointRequest(bpAt);
            bp.addThreadFilter(main);
            bp.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            bp.enable();
            Location hitAt = await(BreakpointEvent.class);
            System.out.println("  breakpoint hit: " + (hitAt != null));
            if (hitAt == null) {
                erm.deleteEventRequest(bp);
                end(type, null);
                return;
            }
            System.out.println("  in: " + hitAt.method().name());
            erm.deleteEventRequest(bp);

            System.out.println("== fifty steps over");
            int completed = 0;
            boolean allInLoop = true;
            TreeSet<Integer> seen = new TreeSet<>();
            for (int k = 0; k < 50; k++) {
                StepRequest step = erm.createStepRequest(main, StepRequest.STEP_LINE,
                        StepRequest.STEP_OVER);
                step.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
                step.enable();
                main.resume();
                Location at = await(StepEvent.class);
                erm.deleteEventRequest(step);
                if (at == null) {
                    break;
                }
                completed++;
                allInLoop &= at.method().name().equals("loop");
                seen.add(at.lineNumber() - bpLine);
            }
            System.out.println("  steps completed: " + completed);
            System.out.println("  every step in loop: " + allInLoop);
            System.out.println("  lines from the breakpoint: " + seen);
            end(type, completed == 50 ? main : null);
        }

        /** Let the loop end and wait for the debuggee to go. */
        static void end(ReferenceType type, ThreadReference suspended) throws Exception {
            System.out.println("== end");
            ((ClassType) type).setValue(type.fieldByName("stop"), vm.mirrorOf(1));
            if (suspended != null) {
                suspended.resume();
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
         * The location of the first event of `kind` within 30 s (its thread
         * stays suspended), or null.
         */
        static Location await(Class<? extends LocatableEvent> kind) throws Exception {
            long until = System.currentTimeMillis() + 30_000;
            while (System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(500);
                if (set == null) {
                    continue;
                }
                Location at = null;
                for (Event e : set) {
                    if (kind.isInstance(e)) {
                        at = ((LocatableEvent) e).location();
                    } else if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        fail("the debuggee ended early");
                    }
                }
                if (at != null) {
                    return at;
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
