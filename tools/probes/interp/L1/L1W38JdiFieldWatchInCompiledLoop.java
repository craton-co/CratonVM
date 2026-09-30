// Interpreter round i1 wave 38, lane L1: a JDWP field ACCESS watch set
// while a compiled loop reads the field through a compiled callee, for the
// "baked direct CALLs" item of
// docs/internal/fixed-bugs/interpreter-L5-jvmti-frames-already-compiled-finish-compiled-FIXED-20261005.md
// (before wave 38 a field watch withdrew nothing).
//
// `loop()` reads `Holder.value` of one object through `Helper.read(h)` and
// sleeps 2 ms per iteration; it is warmed first (2000 short calls) so that
// `loop` and `Helper.read` are compiled before the debugger attaches. The
// debugger sets an access watchpoint on `Holder.value`, filtered to the main
// thread (SUSPEND_EVENT_THREAD), and prints whether the thread reported an
// access within 30 s, in which method, and that method's caller.
//
// Canonical transcript: no ids, no addresses, no timings.
// Run by the conformance runner, tools/jdi/run-jdi-conformance.sh
// (`--scenario L1W38JdiFieldWatchInCompiledLoop`).
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   sum>0=true
//
// Under a debugger (the debuggee waits up to five minutes):
//
//   javac -g -d out L1W38JdiFieldWatchInCompiledLoop.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L1W38JdiFieldWatchInCompiledLoop wait
//   #   (a CratonVM built with --features cratonvm-vm/experimental-debug;
//   #    HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5791 -cp out L1W38JdiFieldWatchInCompiledLoop wait)
//   java -cp out L1W38JdiFieldWatchInCompiledLoop debug 5791 > transcript.txt     # HotSpot 25's java
//
// HotSpot 25.0.3 as the debuggee (the debugger's stdout; three runs, the
// same each time):
//
//   == attach
//   == access watch on a field a compiled loop reads
//     access hit: true
//     in: read
//     caller: loop
//   == end
//     disconnected
//
// CratonVM before wave 38 (from reading the code): in the JIT modes a field
// watch made the doors refuse every new compiled entry
// (`DebuggerGates::requires_interpreter`) and asked the loop exits, but
// `loop`'s compiled body, whose thread is in `Thread.sleep` whenever a pause
// comes, kept running and read the field in its compiled `Helper.read` (a
// baked call or a splice), which posts no field event: `access hit: false`.
// Since wave 38 the first field watch withdraws every compiled body, as a
// step or method event request does (`debug::publish_debugger_gates`,
// `interpreter::note_every_method_needs_the_interpreter`), so the loop leaves
// at its next back edge and the access is interpreted. Expected to match
// HotSpot in all four modes; `CRATONVM_DBG_JITC=1` prints
// `[cratonvm-jitc] interpreter-only withdrawal: source=jdwp ...`.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W38JdiFieldWatchInCompiledLoop {
    /** 0 while the loop may run; the debugger sets 1 to end it. */
    static volatile int stop;
    static volatile boolean debugged;
    static long sum;

    static final class Holder {
        int value = 7;
    }

    static final class Helper {
        static int read(Holder h) {
            return h.value;
        }
    }

    static long loop(Holder h, boolean sleepy) throws InterruptedException {
        long s = 0;
        long until = System.currentTimeMillis() + 300_000;
        for (int i = 0; ; i++) {
            s += Helper.read(h);
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
        Holder h = new Holder();
        for (int n = 0; n < 2000; n++) {
            sum += loop(h, false);
        }
        // Let a background compile of `loop` land before the call that stays.
        Thread.sleep(1000);
        sum += loop(h, true);
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
            ReferenceType type = awaitClass("L1W38JdiFieldWatchInCompiledLoop");
            ReferenceType holder = awaitClass("L1W38JdiFieldWatchInCompiledLoop$Holder");
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

            System.out.println("== access watch on a field a compiled loop reads");
            AccessWatchpointRequest watch =
                    erm.createAccessWatchpointRequest(holder.fieldByName("value"));
            watch.addThreadFilter(main);
            watch.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            watch.enable();
            ThreadReference hit = null;
            Location at = null;
            long until = System.currentTimeMillis() + 30_000;
            while (hit == null && System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(500);
                if (set == null) {
                    continue;
                }
                for (Event e : set) {
                    if (e instanceof AccessWatchpointEvent w) {
                        hit = w.thread();
                        at = w.location();
                    } else if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        fail("the debuggee ended early");
                    }
                }
                if (hit == null) {
                    set.resume();
                }
            }
            System.out.println("  access hit: " + (hit != null));
            if (hit != null) {
                System.out.println("  in: " + at.method().name());
                System.out.println("  caller: " + hit.frame(1).location().method().name());
            }
            erm.deleteEventRequest(watch);

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
