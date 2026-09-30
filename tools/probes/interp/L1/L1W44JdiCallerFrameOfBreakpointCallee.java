// Interpreter round i1 wave 44, lane L1: StackFrame.GetValues / SetValues on
// the CALLER of the method a breakpoint stops in, when that caller is hot
// enough to be compiled (the proposal
// docs/known-issues/interpreter/i43-L1-proposal-deoptimize-a-compiled-frame-for-the-frame-commands-20261007.md).
//
// The debuggee waits (`go`) until the debugger has set a breakpoint on the
// rare branch of `callee` (taken once per 1000 calls, SUSPEND_EVENT_THREAD,
// main thread), then runs `outer()`: 200,000 calls of `mid(i)`, which keeps
// two locals (`x`, its parameter, and `bias`, 0) and calls `callee(x)`. `mid`
// is small and hot, so a JIT compiles it while the breakpoint is in force.
// At every hit the debugger reads `x` and `bias` of frame 1 (`mid`), checks
// `x` against `callee`'s own `x`, and writes `bias = 1` into frame 1. After
// the loop `main` calls `done(sum)`, where a second breakpoint reads the sum:
// every write that reached `mid` added 1 to it.
//
// Canonical transcript: no ids, no addresses, no timings.
// Run by the conformance runner, tools/jdi/run-jdi-conformance.sh
// (`--scenario L1W44JdiCallerFrameOfBreakpointCallee`).
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   sum=19999900000
//
// Under a debugger (the debuggee waits up to five minutes):
//
//   javac -g -d out L1W44JdiCallerFrameOfBreakpointCallee.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W44JdiCallerFrameOfBreakpointCallee wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5791 -cp out L1W44JdiCallerFrameOfBreakpointCallee wait)
//   java -cp out L1W44JdiCallerFrameOfBreakpointCallee debug 5791 > transcript.txt     # HotSpot 25's java
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout; three runs on the
// Windows box, the same each time; `mid` is C1/C2-compiled long before the
// last hits, and HotSpot reads and writes its frame all the same):
//
//   == attach
//   == breakpoint in callee, frame 1 read and written at every hit
//     hits: 200
//     its caller: mid
//     caller reads that matched: 200
//     caller writes: 200
//     caller refusals: none
//   == the sum after the writes
//     sum: 19999900200
//   == end
//     disconnected
//
// CratonVM before wave 44 (from reading the code; the orchestrator's run is
// the measurement): in the JIT modes `mid` is compiled after the breakpoint
// was set (wave 39 refuses only `callee`, the method the breakpoint sits in),
// so from then on the stop lists `mid` as a compiled activation
// (`interpreter::push_compiled_row`), whose locals answer OPAQUE_FRAME: JDI
// throws `OpaqueFrameException` from `getValues` / `setValue`, the reads and
// writes stop matching after the first few hits, `caller refusals:` names
// `OpaqueFrameException`, and the sum is short of 19999900200 by the number
// of refused writes. `--nojit` matches HotSpot (nothing is compiled).
// Since wave 44 the compile doors also refuse a method that directly invokes
// a method a breakpoint sits in (`interpreter::breakpoint_bars_compiling`,
// `jvmti_events::calls_a_breakpoint_method`), so `mid` stays interpreted and
// its frame is an interpreter frame at every hit. Positive control (JIT
// modes; absent on the base):
// `CRATONVM_DBG_JITC=1` prints
// `[cratonvm-jitc] debugger keeps the caller of a breakpoint method interpreted: L1W44JdiCallerFrameOfBreakpointCallee.mid(I)I`
// once (the first time a compile door asks about `mid` after the
// breakpoints were set); with the switch
// `BREAKPOINT_CALLERS_STAY_INTERPRETED_ENABLED` false it never prints and the
// transcript is the base's.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W44JdiCallerFrameOfBreakpointCallee {
    /** 0 until the debugger has set its breakpoints. */
    static volatile int go;

    static int callee(int x) {
        if (x % 1000 == 999) {
            return x + 2; // BREAKPOINT_LINE
        }
        return x + 1;
    }

    static int mid(int x) {
        int bias = 0;
        int r = callee(x);
        return r - 1 + bias;
    }

    static long outer() {
        long s = 0;
        for (int i = 0; i < 200_000; i++) {
            s += mid(i);
        }
        return s;
    }

    /** Where the debugger's second breakpoint reads the sum. */
    static void done(long sum) {
        System.out.println("sum=" + sum);
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
        done(outer() - 200);
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
            ReferenceType type = awaitClass("L1W44JdiCallerFrameOfBreakpointCallee");
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
            Method done = type.methodsByName("done").get(0);

            System.out.println("== breakpoint in callee, frame 1 read and written at every hit");
            BreakpointRequest bp = erm.createBreakpointRequest(callee.locationsOfLine(bpLine).get(0));
            bp.addThreadFilter(main);
            bp.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            bp.enable();
            BreakpointRequest end = erm.createBreakpointRequest(done.location());
            end.addThreadFilter(main);
            end.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            end.enable();
            ((ClassType) type).setValue(type.fieldByName("go"), vm.mirrorOf(1));
            int hits = 0;
            int matched = 0;
            int writes = 0;
            String caller = "-";
            TreeSet<String> refusals = new TreeSet<>();
            String sum = "-";
            long until = System.currentTimeMillis() + 240_000;
            boolean ended = false;
            boolean finished = false;
            while (!ended && !finished && System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(2_000);
                if (set == null) {
                    continue;
                }
                for (Event e : set) {
                    if (e instanceof BreakpointEvent b && b.request() == bp) {
                        hits++;
                        StackFrame top = b.thread().frame(0);
                        int x = ((IntegerValue) top.getValue(top.visibleVariableByName("x"))).value();
                        StackFrame f1 = b.thread().frame(1);
                        caller = f1.location().method().name();
                        try {
                            LocalVariable cx = f1.visibleVariableByName("x");
                            LocalVariable bias = f1.visibleVariableByName("bias");
                            Map<LocalVariable, Value> values = f1.getValues(List.of(cx, bias));
                            int readX = ((IntegerValue) values.get(cx)).value();
                            int readBias = ((IntegerValue) values.get(bias)).value();
                            if (readX == x && readBias == 0) {
                                matched++;
                            }
                            f1.setValue(bias, vm.mirrorOf(1));
                            writes++;
                        } catch (RuntimeException | AbsentInformationException ex) {
                            refusals.add(ex.getClass().getSimpleName());
                        }
                    } else if (e instanceof BreakpointEvent b && b.request() == end) {
                        StackFrame top = b.thread().frame(0);
                        sum = String.valueOf(top.getValue(top.visibleVariableByName("sum")));
                        finished = true;
                        // Before the resume: the debuggee ends right after it.
                        erm.deleteEventRequest(bp);
                        erm.deleteEventRequest(end);
                    } else if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        ended = true;
                    }
                }
                if (!ended) {
                    set.resume();
                }
            }
            System.out.println("  hits: " + hits);
            System.out.println("  its caller: " + caller);
            System.out.println("  caller reads that matched: " + matched);
            System.out.println("  caller writes: " + writes);
            System.out.println("  caller refusals: " + (refusals.isEmpty() ? "none" : String.join(" ", refusals)));
            System.out.println("== the sum after the writes");
            System.out.println("  sum: " + sum);

            System.out.println("== end");
            if (!ended && !finished) {
                erm.deleteEventRequest(bp);
                erm.deleteEventRequest(end);
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
