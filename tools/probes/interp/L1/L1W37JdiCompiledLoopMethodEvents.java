// Interpreter round i1 wave 37, lane L1: the JDI conformance harness's
// "compiled loop" scenario, the positive control of the interpreter-only
// withdrawal (`jvmti_events::note_every_method_needs_the_interpreter`) for
// docs/internal/fixed-bugs/interpreter-L1-jdwp-method-events-and-stop-miss-native-and-compiled-code-FIXED-20261005.md
// (method events of Java methods running compiled),
// docs/internal/fixed-bugs/interpreter-L1-methods-served-without-a-frame-report-no-method-events-FIXED-20261004.md
// (a compiled caller of a method an intrinsic stands in for),
// docs/internal/fixed-bugs/interpreter-L5-jvmti-frames-already-compiled-finish-compiled-FIXED-20261005.md
// (baked direct calls) and
// docs/internal/fixed-bugs/interpreter-L1-jdwp-suspension-does-not-reach-compiled-or-native-code-FIXED-20261005.md
// (a thread in compiled code that no pause reaches).
//
// `loop(true)` runs a counted loop that calls `callee(i)` and
// `text.length()` and then sleeps 5 ms, so its thread spends nearly all its
// time blocked in `Thread.sleep`. The loop is warmed first (2000 short calls
// of `loop(false)`) so that it, `callee` and `String.length` are compiled
// before the debugger attaches, and `loop(true)` is entered compiled. While it
// runs, the debugger:
//
//  * creates a MethodEntryRequest filtered to the main thread and the probe
//    class (SUSPEND_NONE) and waits for three entries of `callee`;
//  * does the same filtered to java.lang.String and waits for three entries
//    of `String.length`;
//  * suspends main, creates a LINE / INTO step filtered to the probe class
//    (count 1, SUSPEND_EVENT_THREAD), resumes main and waits for the step;
//  * then lets the loop end.
//
// A row prints `true` when its event arrived within 30 s.
//
// Canonical transcript: no ids, no addresses, no timings.
// Run by the conformance runner, tools/jdi/run-jdi-conformance.sh
// (`--scenario L1W37JdiCompiledLoopMethodEvents`); the JIT modes are the ones
// that matter (`--modes "jdk-only compatible"`), `--nojit` must print the same.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   sum>0=true
//
// Under a debugger (the debuggee waits up to five minutes):
//
//   javac -g -d out L1W37JdiCompiledLoopMethodEvents.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W37JdiCompiledLoopMethodEvents wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5791 -cp out L1W37JdiCompiledLoopMethodEvents wait)
//   java -cp out L1W37JdiCompiledLoopMethodEvents debug 5791 > transcript.txt     # HotSpot 25's java
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout; three runs, the
// same each time):
//
//   == attach
//   == method entries from the compiled loop
//     callee entered: true
//     String.length entered: true
//   == a step from the compiled loop
//     stepped into the probe class: true
//   == end
//     disconnected
//
// CratonVM before wave 37 (from reading the code; the orchestrator's run is
// the measurement): `loop` runs compiled when the requests arm; the doors
// refuse only NEW entries, the loop-exit pause does not reach a thread
// blocked in `Thread.sleep`, and the loop's calls of `callee` and
// `String.length` are a baked direct call and an intrinsic (or inlined), so
// every row prints `false` in the JIT modes (`true` under `--nojit`). Since
// wave 37 the MethodEntry request (and the step) withdraws every compiled
// body: `loop`'s next back edge leaves for the interpreter (its exit polls are
// forced), and every row prints `true`.
//
// Positive control on the host: `CRATONVM_DBG_JITC=1` on the debuggee prints
//   [cratonvm-jitc] interpreter-only withdrawal: source=jdwp evicted=<n> not-entrant=<n> exits-forced=<n>
// once per withdrawal (three here: each request arms it again after the
// previous one was deleted), and `CRATONVM_DBG_DEOPT=1` prints
//   [cratonvm-deopt] withdrawn body told to leave: L1W37JdiCompiledLoopMethodEvents.loop...
// when the running loop leaves.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W37JdiCompiledLoopMethodEvents {
    /** 0 while the loop may run; the debugger sets 1 to end it. */
    static volatile int stop;
    static volatile boolean debugged;
    static String text = "compiled";
    static long sum;

    static int callee(int x) {
        return x * 3 + 1;
    }

    static long loop(boolean sleepy) throws InterruptedException {
        long s = 0;
        long until = System.currentTimeMillis() + 300_000;
        for (int i = 0; ; i++) {
            s += callee(i);
            s += text.length();
            if (!sleepy) {
                if (i == 500) {
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
            type = awaitClass("L1W37JdiCompiledLoopMethodEvents");
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals("main")) {
                    main = t;
                }
            }
            if (main == null) {
                fail("no main thread");
            }
            // The warm-up (2000 short loops and a 1 s sleep) is over, and the
            // sleepy loop runs, compiled when the JIT is on.
            Thread.sleep(4000);

            System.out.println("== method entries from the compiled loop");
            MethodEntryRequest own = erm.createMethodEntryRequest();
            own.addThreadFilter(main);
            own.addClassFilter("L1W37JdiCompiledLoopMethodEvents");
            own.setSuspendPolicy(EventRequest.SUSPEND_NONE);
            own.enable();
            System.out.println("  callee entered: " + awaitEntries("callee", 3));
            erm.deleteEventRequest(own);

            MethodEntryRequest strings = erm.createMethodEntryRequest();
            strings.addThreadFilter(main);
            strings.addClassFilter("java.lang.String");
            strings.setSuspendPolicy(EventRequest.SUSPEND_NONE);
            strings.enable();
            System.out.println("  String.length entered: " + awaitEntries("length", 3));
            erm.deleteEventRequest(strings);

            System.out.println("== a step from the compiled loop");
            main.suspend();
            StepRequest step = erm.createStepRequest(main, StepRequest.STEP_LINE,
                    StepRequest.STEP_INTO);
            step.addClassFilter("L1W37JdiCompiledLoopMethodEvents");
            step.addCountFilter(1);
            step.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            step.enable();
            main.resume();
            boolean stepped = awaitStep();
            System.out.println("  stepped into the probe class: " + stepped);
            erm.deleteEventRequest(step);

            System.out.println("== end");
            ((ClassType) type).setValue(type.fieldByName("stop"), vm.mirrorOf(1));
            if (stepped) {
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

        /** Did `count` entries of a method named `name` arrive within 30 s? */
        static boolean awaitEntries(String name, int count) throws Exception {
            long until = System.currentTimeMillis() + 30_000;
            int seen = 0;
            while (System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(500);
                if (set == null) {
                    continue;
                }
                for (Event e : set) {
                    if (e instanceof MethodEntryEvent me && me.method().name().equals(name)) {
                        seen++;
                    } else if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        fail("the debuggee ended early");
                    }
                }
                set.resume();
                if (seen >= count) {
                    return true;
                }
            }
            return false;
        }

        /** Did the step's event arrive within 30 s? The thread stays suspended. */
        static boolean awaitStep() throws Exception {
            long until = System.currentTimeMillis() + 30_000;
            while (System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(500);
                if (set == null) {
                    continue;
                }
                for (Event e : set) {
                    if (e instanceof StepEvent) {
                        return true;
                    } else if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        fail("the debuggee ended early");
                    }
                }
                set.resume();
            }
            return false;
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
