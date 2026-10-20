// Interpreter round i6 wave 124, lane L2: the positive control of stages 2
// and 3b of
// docs/internal/proposals/interpreter/i43-L1-proposal-deoptimize-a-compiled-frame-for-the-frame-commands-20261007.md
// -- StackFrame.GetValues and SetValues on a caller that was COMPILED, under
// the debugger, BEFORE the breakpoint in its callee was set.
//
// Wave 44 keeps a caller compiled after a breakpoint interpreted
// (L1W44JdiCallerFrameOfBreakpointCallee), so this scenario sets the
// breakpoint only after the caller is warm. The debugger attaches first
// (`go`), so every compile of `mid` is made while a debugger is attached
// (`BackendRequest::debugger_observes_locals`). The debuggee then runs
// phase 1, 500,000 calls of `mid(i)`, and says `warm`. The debugger sets a
// breakpoint on the rare branch of `HookC.rare` (main thread,
// SUSPEND_EVENT_THREAD) and says `go2`; phase 2 runs 200,000 more calls.
//
// `mid` reaches `rare` through a megamorphic interface call (three hooks)
// and is larger than any inlining limit (the cold pad after the call), so
// it is neither inlined into its caller's loop nor a baked caller of
// `HookC`: the breakpoint's class withdrawal leaves its body. At each of the
// 66 hits the debugger reads frame 1 (`mid`): `x`, `bias` (0) and `wide`
// (3x), checks them, writes `bias = 1`, and reads `bias` back. Every write
// that reaches `mid` adds 1 to the phase-2 sum, which a second breakpoint
// reads in `done`.
//
// Canonical transcript: no ids, no addresses, no timings.
// Run by the conformance runner, tools/jdi/run-jdi-conformance.sh
// (`--scenario L2W124JdiCompiledCallerSetValues`; not in the default list).
//
// Plain run (no debugger): stdout must equal HotSpot 25's (default flags and
// -Xint alike):
//
//   sum1=124999750166 sum2=19999900066
//
// Under a debugger (the debuggee waits up to five minutes at each gate):
//
//   javac -g -d out L2W124JdiCompiledCallerSetValues.java
//   cratonvm --java-home $JDK --jdwp-port 5743 -cp out L2W124JdiCompiledCallerSetValues wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5743 -cp out L2W124JdiCompiledCallerSetValues wait)
//   java -cp out L2W124JdiCompiledCallerSetValues debug 5743 > transcript.txt     # HotSpot 25's java
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout; Windows, wave 124;
// the same with an -Xint debuggee):
// HOTSPOT_EXPECTED_BEGIN
//   == attach
//   == warm, then a breakpoint in HookC.rare; frame 1 read and written at every hit
//     hits: 66
//     its caller: mid
//     caller reads that matched: 66
//     caller writes: 66
//     read-backs of the write: 66
//     caller refusals: none
//   == the phase-2 sum after the writes
//     sum2: 19999900132
//   == end
//     disconnected
// HOTSPOT_EXPECTED_END
//
// CratonVM, expected (from reading the code; the orchestrator's run is the
// measurement), JIT modes, `experimental-debug` build:
//
// * `CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL` unset (the default): frame 1 is
//   an opaque compiled row. JDI throws `OpaqueFrameException` from
//   `getValues`: `caller reads that matched: 0`, `caller writes: 0`,
//   `read-backs of the write: 0`, `caller refusals: OpaqueFrameException`,
//   `sum2: 19999900066`.
// * `CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL=1`: stage 2 serves the reads
//   (the 66 matched reads). Stage 3b serves each write when the force
//   verdict for `mid`'s call site is `leaves`: then the transcript is
//   HotSpot's. A refused verdict leaves the writes as `OpaqueFrameException`
//   (reads still match).
//   Positive controls, with `CRATONVM_FRAME_TRACE=1 CRATONVM_DBG_JITC=1`:
//   * `[cratonvm-jitc] debugger home frame ...mid(I)I bci=<b>: write door exit-after-call`
//     (or `refused:<cause>`, the census of wave 122);
//   * `[cratonvm-jitc] debugger home write force ...mid(I)I call-bci=<b>: leaves (...)`;
//   * `[COMPILED_SETVALUES] recorded tid=<n> ...mid(I)I call-bci=<b> exit-bci=<e> slots=1`
//     then `[COMPILED_SETVALUES] applied tid=<n> writes=1 of 1`, once per hit
//     the force reached. The first hit withdraws `mid`'s body (the force's
//     hold), so the later hits may find `mid` interpreted: the transcript
//     still matches, and the trace shows how many hits were compiled.
//   With no `debugger home frame` line, `mid` was not compiled under the
//   debugger before phase 2 (or was inlined, or withdrawn with `HookC`:
//   look for `breakpoint withdrawal: class=...HookC`), and the probe tested
//   nothing compiled.
// * `--nojit`: HotSpot's transcript (nothing is compiled).
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L2W124JdiCompiledCallerSetValues {
    /** 0 until the debugger has attached. */
    static volatile int go;
    /** 1 once phase 1 (the warm-up) is done. */
    static volatile int warm;
    /** 0 until the debugger has set its breakpoints. */
    static volatile int go2;

    interface Hook {
        int rare(int x);
    }

    static final class HookA implements Hook {
        public int rare(int x) {
            return x + 1;
        }
    }

    static final class HookB implements Hook {
        public int rare(int x) {
            return x + 1;
        }
    }

    static final class HookC implements Hook {
        public int rare(int x) {
            if (x % 1000 == 999) {
                return x + 2; // BREAKPOINT_LINE
            }
            return x + 1;
        }
    }

    static final Hook[] HOOKS = {new HookA(), new HookB(), new HookC()};

    static int mid(int x) {
        int bias = 0;
        long wide = 3L * x;
        int r = HOOKS[x % 3].rare(x);
        if (x < 0) {
            // Never run: only here to put `mid` past every inlining limit.
            int t = r;
            t = t * 31 + 1;
            t = t * 31 + 4;
            t = t * 31 + 7;
            t = t * 31 + 10;
            t = t * 31 + 13;
            t = t * 31 + 16;
            t = t * 31 + 19;
            t = t * 31 + 22;
            t = t * 31 + 25;
            t = t * 31 + 28;
            t = t * 31 + 31;
            t = t * 31 + 34;
            t = t * 31 + 37;
            t = t * 31 + 40;
            t = t * 31 + 43;
            t = t * 31 + 46;
            t = t * 31 + 49;
            t = t * 31 + 52;
            t = t * 31 + 55;
            t = t * 31 + 58;
            t = t * 31 + 61;
            t = t * 31 + 64;
            t = t * 31 + 67;
            t = t * 31 + 70;
            t = t * 31 + 73;
            t = t * 31 + 76;
            t = t * 31 + 79;
            t = t * 31 + 82;
            t = t * 31 + 85;
            t = t * 31 + 88;
            t = t * 31 + 91;
            t = t * 31 + 94;
            t = t * 31 + 97;
            t = t * 31 + 100;
            t = t * 31 + 103;
            t = t * 31 + 106;
            t = t * 31 + 109;
            t = t * 31 + 112;
            t = t * 31 + 115;
            t = t * 31 + 118;
            bias += t;
        }
        return r - 1 + bias + (int) (wide - 3L * x);
    }

    static long outer(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += mid(i);
        }
        return s;
    }

    /** Where the debugger's second breakpoint reads the phase-2 sum. */
    static void done(long sum1, long sum2) {
        System.out.println("sum1=" + sum1 + " sum2=" + sum2);
    }

    static void await(boolean debugged, String what) throws InterruptedException {
        if (!debugged) {
            return;
        }
        long until = System.currentTimeMillis() + 300_000;
        while ((what.equals("go") ? go : go2) == 0 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean debugged = args.length == 1 && args[0].equals("wait");
        await(debugged, "go");
        long sum1 = outer(500_000);
        warm = 1;
        await(debugged, "go2");
        long sum2 = outer(200_000);
        done(sum1, sum2);
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
            ReferenceType type = awaitClass("L2W124JdiCompiledCallerSetValues");
            ThreadReference main = null;
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals("main")) {
                    main = t;
                }
            }
            if (main == null) {
                fail("no main thread");
            }
            ((ClassType) type).setValue(type.fieldByName("go"), vm.mirrorOf(1));
            // Phase 1 runs with the debugger attached and no request.
            Field warmField = type.fieldByName("warm");
            long warmUntil = System.currentTimeMillis() + 240_000;
            while (((IntegerValue) type.getValue(warmField)).value() == 0) {
                if (System.currentTimeMillis() > warmUntil) {
                    fail("the debuggee never got warm");
                }
                Thread.sleep(20);
            }
            ReferenceType hookC = awaitClass("L2W124JdiCompiledCallerSetValues$HookC");
            Method rare = hookC.methodsByName("rare").get(0);
            // The line of `return x + 2;`: the second distinct line of `rare`.
            TreeSet<Integer> lines = new TreeSet<>();
            for (Location l : rare.allLineLocations()) {
                lines.add(l.lineNumber());
            }
            int bpLine = new ArrayList<>(lines).get(1);
            Method done = type.methodsByName("done").get(0);

            System.out.println("== warm, then a breakpoint in HookC.rare; frame 1 read and written at every hit");
            BreakpointRequest bp = erm.createBreakpointRequest(rare.locationsOfLine(bpLine).get(0));
            bp.addThreadFilter(main);
            bp.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            bp.enable();
            BreakpointRequest end = erm.createBreakpointRequest(done.location());
            end.addThreadFilter(main);
            end.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            end.enable();
            ((ClassType) type).setValue(type.fieldByName("go2"), vm.mirrorOf(1));
            int hits = 0;
            int matched = 0;
            int writes = 0;
            int readBacks = 0;
            String caller = "-";
            TreeSet<String> refusals = new TreeSet<>();
            String sum2 = "-";
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
                            LocalVariable wide = f1.visibleVariableByName("wide");
                            Map<LocalVariable, Value> values = f1.getValues(List.of(cx, bias, wide));
                            int readX = ((IntegerValue) values.get(cx)).value();
                            int readBias = ((IntegerValue) values.get(bias)).value();
                            long readWide = ((LongValue) values.get(wide)).value();
                            if (readX == x && readBias == 0 && readWide == 3L * x) {
                                matched++;
                            }
                            f1.setValue(bias, vm.mirrorOf(1));
                            writes++;
                            // The frame object of the same stop, read again.
                            StackFrame again = b.thread().frame(1);
                            int back = ((IntegerValue) again.getValue(again.visibleVariableByName("bias"))).value();
                            if (back == 1) {
                                readBacks++;
                            }
                        } catch (RuntimeException | AbsentInformationException ex) {
                            refusals.add(ex.getClass().getSimpleName());
                        }
                    } else if (e instanceof BreakpointEvent b && b.request() == end) {
                        StackFrame top = b.thread().frame(0);
                        sum2 = String.valueOf(top.getValue(top.visibleVariableByName("sum2")));
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
            System.out.println("  read-backs of the write: " + readBacks);
            System.out.println("  caller refusals: " + (refusals.isEmpty() ? "none" : String.join(" ", refusals)));
            System.out.println("== the phase-2 sum after the writes");
            System.out.println("  sum2: " + sum2);

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
