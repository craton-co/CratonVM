// Interpreter round i1 wave 39, lane L1: a breakpoint set BEFORE the callers
// of its method are compiled, for item 3 of
// docs/internal/fixed-bugs/interpreter-L1-a-breakpoint-in-a-compiled-callee-of-a-running-compiled-caller-is-missed-FIXED-20261004.md
// (a caller compiled after the breakpoint was set may splice its method, or
// bake a raw CALL into a body of it compiled after it, and miss the hits).
//
// The debuggee waits (`go`) until the debugger has set a breakpoint on the
// rare branch of `callee` (taken once per 1000 calls, SUSPEND_EVENT_THREAD,
// main thread), then runs `outer()`: 200,000 calls of `mid(i)`, each calling
// `callee(i)`. `mid` and `callee` are small and hot, so the JIT compiles
// `mid` (and would splice `callee` into it, or bind it directly) while the
// breakpoint is in force. The debugger counts the hits, resuming after each,
// and prints how many arrived (every one of the 200 must), where the last
// one stopped and its caller.
//
// Canonical transcript: no ids, no addresses, no timings.
// Run by the conformance runner, tools/jdi/run-jdi-conformance.sh
// (`--scenario L1W39JdiBreakpointBeforeCallersCompile`).
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   sum=20000100200
//
// Under a debugger (the debuggee waits up to five minutes):
//
//   javac -g -d out L1W39JdiBreakpointBeforeCallersCompile.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W39JdiBreakpointBeforeCallersCompile wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5791 -cp out L1W39JdiBreakpointBeforeCallersCompile wait)
//   java -cp out L1W39JdiBreakpointBeforeCallersCompile debug 5791 > transcript.txt     # HotSpot 25's java
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout; three runs, the
// same each time):
//
//   == attach
//   == breakpoint set before the callers compile
//     hits: 200
//     last hit in: callee
//     its caller: mid
//   == end
//     disconnected
//
// CratonVM before wave 39 (from reading the code; the orchestrator's run is
// the measurement): in the JIT modes the tier-up stride offered `callee`
// and `mid`, and `mid`'s compile spliced `callee` (the inline resolver never
// asked the debugger) or bound a raw CALL into `callee`'s new body, so the
// hits after `mid` compiled were missed (`hits` below 200). Since wave 39 the
// compile doors refuse a method a breakpoint sits in
// (`interpreter::breakpoint_bars_compiling`): the stride does not offer it,
// the by-name compile builds no body of it, and the inline resolver does not
// splice it. `CRATONVM_DBG_JITC=1` prints
// `[cratonvm-jitc] inline-resolve REFUSED L1W39JdiBreakpointBeforeCallersCompile.callee(I)I depth=0: debugger-breakpoint`
// when `mid` is compiled (the positive control). `--nojit` must print the
// same.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W39JdiBreakpointBeforeCallersCompile {
    /** 0 until the debugger has set its breakpoint. */
    static volatile int go;
    static long sum;

    static int callee(int x) {
        if (x % 1000 == 999) {
            return x + 2; // BREAKPOINT_LINE
        }
        return x + 1;
    }

    static int mid(int x) {
        return callee(x);
    }

    static long outer() {
        long s = 0;
        for (int i = 0; i < 200_000; i++) {
            s += mid(i);
        }
        return s;
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
        sum = outer();
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
            EventRequestManager erm = vm.eventRequestManager();
            System.out.println("== attach");
            ReferenceType type = awaitClass("L1W39JdiBreakpointBeforeCallersCompile");
            ThreadReference main = null;
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals("main")) {
                    main = t;
                }
            }
            if (main == null) {
                fail("no main thread");
            }
            Method callee = type.methodsByName("callee").get(0);
            // The line of `return x + 2;`: the second distinct line of
            // `callee` (after the `if`).
            TreeSet<Integer> lines = new TreeSet<>();
            for (Location l : callee.allLineLocations()) {
                lines.add(l.lineNumber());
            }
            int bpLine = new ArrayList<>(lines).get(1);

            System.out.println("== breakpoint set before the callers compile");
            BreakpointRequest bp = erm.createBreakpointRequest(callee.locationsOfLine(bpLine).get(0));
            bp.addThreadFilter(main);
            bp.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            bp.enable();
            ((ClassType) type).setValue(type.fieldByName("go"), vm.mirrorOf(1));
            int hits = 0;
            String lastIn = "-";
            String lastCaller = "-";
            long until = System.currentTimeMillis() + 120_000;
            boolean ended = false;
            while (!ended && System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(2_000);
                if (set == null) {
                    // No hit for two seconds: the loop is over (or every later
                    // hit was missed).
                    if (hits > 0) {
                        break;
                    }
                    continue;
                }
                for (Event e : set) {
                    if (e instanceof BreakpointEvent b) {
                        hits++;
                        lastIn = b.location().method().name();
                        lastCaller = b.thread().frame(1).location().method().name();
                    } else if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        ended = true;
                    }
                }
                if (!ended) {
                    set.resume();
                }
            }
            System.out.println("  hits: " + hits);
            System.out.println("  last hit in: " + lastIn);
            System.out.println("  its caller: " + lastCaller);

            System.out.println("== end");
            if (!ended) {
                erm.deleteEventRequest(bp);
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
