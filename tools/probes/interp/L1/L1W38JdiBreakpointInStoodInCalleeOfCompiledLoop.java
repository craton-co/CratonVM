// Interpreter round i1 wave 38, lane L1: a JDWP breakpoint in a JDK method
// the interpreter answers without a frame (`String.length()`, an intrinsic
// entry), set while a compiled loop calls it, for item 1 of
// docs/internal/fixed-bugs/interpreter-L1-methods-served-without-a-frame-report-no-method-events-FIXED-20261004.md
// and the scoped withdrawal of
// docs/internal/fixed-bugs/interpreter-L1-a-breakpoint-in-a-compiled-callee-of-a-running-compiled-caller-is-missed-FIXED-20261004.md.
//
// `loop()` adds `word.length()` and sleeps 2 ms per iteration; it is warmed
// first (2000 short calls) so that it is compiled before the debugger
// attaches, with the JIT's `String.length` intrinsic at the call. The
// debugger sets a breakpoint at `String.length()`'s first location, filtered
// to the main thread (SUSPEND_EVENT_THREAD), and prints whether the thread
// stopped there within 30 s, in which method and at which index, and in which
// method its caller frame is.
//
// Canonical transcript: no ids, no addresses, no timings.
// Run by the conformance runner, tools/jdi/run-jdi-conformance.sh
// (`--scenario L1W38JdiBreakpointInStoodInCalleeOfCompiledLoop`).
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   sum>0=true
//
// Under a debugger (the debuggee waits up to five minutes):
//
//   javac -g -d out L1W38JdiBreakpointInStoodInCalleeOfCompiledLoop.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W38JdiBreakpointInStoodInCalleeOfCompiledLoop wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5791 -cp out L1W38JdiBreakpointInStoodInCalleeOfCompiledLoop wait)
//   java -cp out L1W38JdiBreakpointInStoodInCalleeOfCompiledLoop debug 5791 > transcript.txt     # HotSpot 25's java
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout; three runs, the
// same each time):
//
//   == attach
//   == breakpoint in String.length under a compiled loop
//     breakpoint hit: true
//     method: java.lang.String.length index=0
//     caller: loop
//   == end
//     disconnected
//
// CratonVM before wave 38 (from reading the code): in the JIT modes
// `breakpoint hit: false` (the compiled loop's `String.length` intrinsic ran
// on; a breakpoint withdrew nothing). Since wave 38 a breakpoint gained in a
// JDK class withdraws every compiled body
// (`interpreter::note_breakpoint_classes_gained`: the JIT's intrinsics expand
// JDK methods without a record the scoped withdrawal could read), so the loop
// leaves at its next back edge, and its next `String.length` call reaches the
// native-call funnel from an interpreter frame, whose hook runs the method's
// bytecode under the breakpoint (`run_stood_in_java_method`). Expected to
// match HotSpot in all four modes; `CRATONVM_DBG_JITC=1` prints
// `[cratonvm-jitc] breakpoint withdrawal: class=java/lang/String ... scoped=false`.
// Since wave 40 a breakpoint in a JDK class keeps every method interpreted
// while it stands and withdraws every body first
// (`debug::WITHDRAWAL_BY_JDK_BREAKPOINT`), so the line is
// `[cratonvm-jitc] interpreter-only withdrawal: source=jdk-breakpoint ...`
// instead (the scoped withdrawal then finds every body withdrawn).
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W38JdiBreakpointInStoodInCalleeOfCompiledLoop {
    /** 0 while the loop may run; the debugger sets 1 to end it. */
    static volatile int stop;
    static volatile boolean debugged;
    static String word = "breakpoint";
    static long sum;

    static long loop(boolean sleepy) throws InterruptedException {
        long s = 0;
        long until = System.currentTimeMillis() + 300_000;
        for (int i = 0; ; i++) {
            s += word.length();
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
            ReferenceType type = awaitClass("L1W38JdiBreakpointInStoodInCalleeOfCompiledLoop");
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

            System.out.println("== breakpoint in String.length under a compiled loop");
            ReferenceType string = vm.classesByName("java.lang.String").get(0);
            Method length = null;
            for (Method m : string.methodsByName("length")) {
                if (m.signature().equals("()I")) {
                    length = m;
                }
            }
            BreakpointRequest bp = erm.createBreakpointRequest(length.location());
            bp.addThreadFilter(main);
            bp.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            bp.enable();
            ThreadReference hit = null;
            Location at = null;
            long until = System.currentTimeMillis() + 30_000;
            while (hit == null && System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(500);
                if (set == null) {
                    continue;
                }
                for (Event e : set) {
                    if (e instanceof BreakpointEvent b) {
                        hit = b.thread();
                        at = b.location();
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
                System.out.println("  method: " + at.declaringType().name() + "."
                        + at.method().name() + " index=" + at.codeIndex());
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
