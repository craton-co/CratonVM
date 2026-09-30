// Interpreter round i1 wave 40, lane L1: a breakpoint in a JDK method the
// JIT expands at a call site from its fixed intrinsic table
// (`String.charAt(I)C`, `JitIntrinsic::StringCharAt`), set BEFORE its caller
// is compiled: the
// remainder of
// docs/internal/fixed-bugs/interpreter-L1-a-breakpoint-in-a-compiled-callee-of-a-running-compiled-caller-is-missed-FIXED-20261004.md
// and item 1 of
// docs/internal/fixed-bugs/interpreter-L1-methods-served-without-a-frame-report-no-method-events-FIXED-20261004.md.
//
// The debuggee waits (`go`) until the debugger has set a breakpoint at
// bytecode index 16 of `String.charAt(int)` (`aload_0` of the UTF-16 branch,
// reached only for a string that is not Latin-1: once per 1000 calls here),
// filtered to the main thread, SUSPEND_EVENT_THREAD. It then runs `outer()`:
// 200,000 calls of `mid(i)`, each `charAt(0)` of a Latin-1 string, or of a
// UTF-16 one when `i % 1000 == 999`. `mid` is small and hot, so the JIT
// compiles it while the breakpoint is in force, and its `charAt` call is a
// call-site intrinsic. The debugger counts the hits
// whose caller is `mid`, resuming after each, and prints how many arrived
// (every one of the 200 must), where the last one stopped and its caller.
//
// Canonical transcript: no ids, no addresses, no timings.
// Run by the conformance runner, tools/jdi/run-jdi-conformance.sh
// (`--scenario L1W40JdiBreakpointInJitIntrinsicCallee`).
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   sum=19432000
//
// Under a debugger (the debuggee waits up to five minutes):
//
//   javac -g -d out L1W40JdiBreakpointInJitIntrinsicCallee.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W40JdiBreakpointInJitIntrinsicCallee wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5791 -cp out L1W40JdiBreakpointInJitIntrinsicCallee wait)
//   java -cp out L1W40JdiBreakpointInJitIntrinsicCallee debug 5791 > transcript.txt     # HotSpot 25's java
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout; three runs, the
// same each time):
//
//   == attach
//   == breakpoint in String.charAt before its caller compiles
//     hits from mid: 200
//     last hit in: java.lang.String.charAt index=16
//     its caller: mid
//   == end
//     disconnected
//
// CratonVM before wave 40 (from reading the code; the orchestrator's run is
// the measurement): in the JIT modes the breakpoint's whole-cache withdrawal
// (wave 38) ran once, when it was set, before `mid` was hot; the tier-up
// stride then offered `mid` (only `String.charAt` itself is refused,
// `interpreter::breakpoint_bars_compiling`), and `mid`'s compile expanded
// `charAt` from the JIT's intrinsic table, which asks no VM resolver, so
// the hits after `mid` compiled were missed (`hits from mid` below 200).
// Since wave 40 a breakpoint in a JDK class's method keeps every method
// interpreted and the tier-up strides closed while it stands
// (`debug::WITHDRAWAL_BY_JDK_BREAKPOINT`,
// `interpreter::JDK_BREAKPOINT_INTERPRETER_ONLY_ENABLED`).
// `CRATONVM_DBG_JITC=1` prints
// `[cratonvm-jitc] interpreter-only withdrawal: source=jdk-breakpoint ...`
// when the breakpoint is set (the positive control). `--nojit` must print
// the same transcript.
//
// Why not `Math.max`: HotSpot itself misses a breakpoint in a method its own
// compilers intrinsify. The same probe with a breakpoint at index 10 of
// `Math.max(JJ)J` (`lload_2`) and `mid` returning `Math.max(998L, x % 1000)`
// printed `hits from mid: 6` on HotSpot 25.0.3 (three runs): the hits before
// `mid` was compiled with its `Math.max` intrinsic. CratonVM since wave 40
// reports all 200 there (more than HotSpot, whose count its own compile
// timing decides). `String.charAt` is a call-site intrinsic of CratonVM's JIT
// and not of HotSpot's, which deoptimizes `mid` and reports every hit.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W40JdiBreakpointInJitIntrinsicCallee {
    /** 0 until the debugger has set its breakpoint. */
    static volatile int go;
    static long sum;

    static final String NARROW = "a";
    static final String WIDE = "ā";

    static long mid(int x) {
        String s = x % 1000 == 999 ? WIDE : NARROW;
        return s.charAt(0);
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
            ReferenceType type = awaitClass("L1W40JdiBreakpointInJitIntrinsicCallee");
            ThreadReference main = null;
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals("main")) {
                    main = t;
                }
            }
            if (main == null) {
                fail("no main thread");
            }
            ReferenceType string = awaitClass("java.lang.String");
            Method charAt = null;
            for (Method m : string.methodsByName("charAt")) {
                if (m.signature().equals("(I)C")) {
                    charAt = m;
                }
            }
            if (charAt == null) {
                fail("no String.charAt(I)C");
            }
            // `aload_0` of the UTF-16 branch.
            Location at = charAt.locationOfCodeIndex(16);
            if (at == null) {
                fail("no location at index 16 of String.charAt(I)C");
            }

            System.out.println("== breakpoint in String.charAt before its caller compiles");
            BreakpointRequest bp = erm.createBreakpointRequest(at);
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
                        String caller = b.thread().frame(1).location().method().name();
                        if (caller.equals("mid")) {
                            hits++;
                            Location l = b.location();
                            lastIn = l.declaringType().name() + "." + l.method().name()
                                    + " index=" + l.codeIndex();
                            lastCaller = caller;
                        }
                    } else if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        ended = true;
                    }
                }
                if (!ended) {
                    set.resume();
                }
            }
            System.out.println("  hits from mid: " + hits);
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
