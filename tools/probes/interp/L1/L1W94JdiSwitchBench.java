// Interpreter round i4 wave 94, lane L1 -- the JDI bench that isolates the
// cost each of the two wave-93 debugger switches removes:
//
//   methods     CRATONVM_JDWP_SCOPED_METHOD_EVENTS (page
//               docs/internal/fixed-bugs/interpreter-i39-L1-proposal-scope-class-filtered-method-events-to-their-classes-DONE-20261127.md):
//               an IDE method breakpoint -- a SUSPEND_NONE MethodEntry request
//               filtered by ClassOnly and a MethodExit request filtered by an
//               exact ClassMatch, both on `L1W94Hot`, which the measured loop
//               never calls. Unset: every method of the VM runs interpreted
//               while they stand and every compiled body is withdrawn when
//               they arm; =1: only `L1W94Hot` (and the code that inlined it).
//   exceptions  CRATONVM_JVMTI_EXCEPTION_FROM_COMPILED (page
//               docs/internal/fixed-bugs/interpreter-i10-L1-proposal-exception-and-watch-events-from-compiled-code-DONE-20261224.md):
//               an IDE logging exception breakpoint -- a SUSPEND_NONE
//               Exception request (caught and uncaught) whose ClassMatch names
//               a class that never throws, so no event is sent and what is
//               measured is the running cost of the request standing. Unset:
//               every method runs interpreted; =1: compiled code stays, and a
//               compiled `athrow` records its site and reaches the report.
//
// Each scenario runs five reps; per rep the debugger asks the debuggee to time
// its loop (median of five runs) with no request, arms the request(s) (the
// wall time of `enable()`, which on CratonVM includes the publication and the
// withdrawal of compiled code before the reply), times the loop while they
// stand, disarms them, and times the loop twice more (the first run pays the
// recompile, the second is settled). The `bench` lines are medians over the
// reps; they are excluded from the comparison, every other line is identical
// in every arm.
//
// Run (debug build, `--features experimental-debug`):
//
//   javac -g -d out L1W94JdiSwitchBench.java
//   # CratonVM debuggee (repeat with each switch unset, then =1, in both modes):
//   CRATONVM_JDWP_SCOPED_METHOD_EVENTS=1 cratonvm [--compatible] --java-home $JDK --jdwp-port 5894 -cp out L1W94JdiSwitchBench wait
//   java -cp out L1W94JdiSwitchBench debug 5894 methods
//   CRATONVM_JVMTI_EXCEPTION_FROM_COMPILED=1 cratonvm [--compatible] --java-home $JDK --jdwp-port 5894 -cp out L1W94JdiSwitchBench wait
//   java -cp out L1W94JdiSwitchBench debug 5894 exceptions
//   # HotSpot debuggee (the reference row):
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5894 -cp out L1W94JdiSwitchBench wait
//   java -cp out L1W94JdiSwitchBench debug 5894 methods      (then again with `exceptions`)
//
// Expected A/B (CratonVM): `during` near `before` with the switch, near the
// interpreter's speed (tens of times slower) without it; `arm-us` smaller with
// the methods switch (a scoped withdrawal instead of every body); `after1`
// above `after2` without the switch (the whole program recompiles). Positive
// control: add CRATONVM_DBG_JITC=1 -- with the methods switch one
// `[cratonvm-jitc] method event scope: scopes=2 classes=1` line per arm and no
// `interpreter-only withdrawal: source=jdwp` line.
//
// Plain run (no debugger): stdout must equal HotSpot 25's: `ended`.
// Debugger stdout (either scenario prints its own name in place of <s>):
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3 debuggee, derived from the source)
// == attach
// == <s>
//   reps: 5
// == end
//   disconnected
// HOTSPOT_EXPECTED_END
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W94JdiSwitchBench {
    /** The debugger's request number; -1 ends the debuggee. */
    static volatile int cmd;
    /** The last request number served. */
    static volatile int ack;
    /** What to time: 1 = the compute loop, 2 = the throw-and-catch loop. */
    static volatile int which;
    /** The last timing, ns per 1000 iterations. */
    static volatile long result;
    /** Set once the classes are warm. */
    static volatile boolean warm;
    static volatile long sink;

    public static void main(String[] args) throws Exception {
        if (args.length == 3 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]), args[2]);
            return;
        }
        boolean debugged = args.length == 1 && args[0].equals("wait");
        sink += L1W94Hot.touch();
        for (int round = 0; round < 40; round++) {
            sink += L1W94Compute.run(200_000);
            sink += L1W94Thrower.run(20_000);
        }
        warm = true;
        long until = System.currentTimeMillis() + 600_000;
        while (debugged && System.currentTimeMillis() < until) {
            int c = cmd;
            if (c == -1) {
                break;
            }
            if (c == ack) {
                Thread.sleep(2);
                continue;
            }
            result = which == 2 ? timeThrower() : timeCompute();
            ack = c;
        }
        System.out.println("ended");
    }

    static long timeCompute() {
        final int n = 1_000_000;
        long[] ns = new long[5];
        for (int i = 0; i < ns.length; i++) {
            long t0 = System.nanoTime();
            sink += L1W94Compute.run(n);
            ns[i] = (System.nanoTime() - t0) / (n / 1000);
        }
        Arrays.sort(ns);
        return ns[ns.length / 2];
    }

    static long timeThrower() {
        final int n = 100_000;
        long[] ns = new long[5];
        for (int i = 0; i < ns.length; i++) {
            long t0 = System.nanoTime();
            sink += L1W94Thrower.run(n);
            ns[i] = (System.nanoTime() - t0) / (n / 1000);
        }
        Arrays.sort(ns);
        return ns[ns.length / 2];
    }

    static final class Debugger {
        static VirtualMachine vm;
        static ClassType main;
        static int seq;

        static void run(int port, String scenario) throws Exception {
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
            System.out.println("== attach");
            main = (ClassType) awaitClass("L1W94JdiSwitchBench");
            long until = System.currentTimeMillis() + 300_000;
            while (!((BooleanValue) main.getValue(main.fieldByName("warm"))).value()) {
                if (System.currentTimeMillis() > until) {
                    fail("the debuggee never warmed up");
                }
                Thread.sleep(20);
            }
            EventRequestManager erm = vm.eventRequestManager();
            List<EventRequest> requests = new ArrayList<>();
            List<Integer> kinds = new ArrayList<>();
            if (scenario.equals("methods")) {
                ReferenceType hot = awaitClass("L1W94Hot");
                MethodEntryRequest entries = erm.createMethodEntryRequest();
                entries.addClassFilter(hot);
                entries.setSuspendPolicy(EventRequest.SUSPEND_NONE);
                requests.add(entries);
                MethodExitRequest exits = erm.createMethodExitRequest();
                exits.addClassFilter("L1W94Hot");
                exits.setSuspendPolicy(EventRequest.SUSPEND_NONE);
                requests.add(exits);
                kinds.add(1);
            } else if (scenario.equals("exceptions")) {
                ExceptionRequest thrown = erm.createExceptionRequest(null, true, true);
                thrown.addClassFilter("L1W94NeverThrows");
                thrown.setSuspendPolicy(EventRequest.SUSPEND_NONE);
                requests.add(thrown);
                kinds.add(1);
                kinds.add(2);
            } else {
                fail("unknown scenario " + scenario);
            }
            System.out.println("== " + scenario);
            final int reps = 5;
            String[] rows = {"before", "during", "after1", "after2"};
            Map<String, long[]> samples = new LinkedHashMap<>();
            long[] armUs = new long[reps];
            long[] disarmUs = new long[reps];
            for (int rep = 0; rep < reps; rep++) {
                for (int kind : kinds) {
                    samples.computeIfAbsent(label(kind, "before"), k -> new long[reps])[rep] = measure(kind);
                }
                long t0 = System.nanoTime();
                for (EventRequest r : requests) {
                    r.enable();
                }
                armUs[rep] = (System.nanoTime() - t0) / 1000;
                for (int kind : kinds) {
                    samples.computeIfAbsent(label(kind, "during"), k -> new long[reps])[rep] = measure(kind);
                }
                t0 = System.nanoTime();
                for (EventRequest r : requests) {
                    r.disable();
                }
                disarmUs[rep] = (System.nanoTime() - t0) / 1000;
                for (int kind : kinds) {
                    samples.computeIfAbsent(label(kind, "after1"), k -> new long[reps])[rep] = measure(kind);
                }
                for (int kind : kinds) {
                    samples.computeIfAbsent(label(kind, "after2"), k -> new long[reps])[rep] = measure(kind);
                }
                drainEvents();
            }
            System.out.println("  reps: " + reps);
            StringBuilder line = new StringBuilder("  bench " + scenario);
            for (int kind : kinds) {
                for (String row : rows) {
                    line.append(' ').append(label(kind, row)).append(' ')
                            .append(median(samples.get(label(kind, row))));
                }
            }
            line.append(" arm-us ").append(median(armUs)).append(" disarm-us ").append(median(disarmUs));
            // A `bench` line: excluded from the comparison.
            System.err.println(line);
            for (EventRequest r : requests) {
                erm.deleteEventRequest(r);
            }
            main.setValue(main.fieldByName("cmd"), vm.mirrorOf(-1));
            System.out.println("== end");
            boolean gone = false;
            while (!gone) {
                EventSet next;
                try {
                    next = vm.eventQueue().remove(60_000);
                } catch (VMDisconnectedException disconnected) {
                    break;
                }
                if (next == null) {
                    fail("the debuggee did not end");
                }
                for (Event e : next) {
                    if (e instanceof VMDisconnectEvent) {
                        gone = true;
                    }
                }
                if (!gone) {
                    try {
                        next.resume();
                    } catch (VMDisconnectedException disconnected) {
                        break;
                    }
                }
            }
            System.out.println("  disconnected");
        }

        static String label(int kind, String row) {
            return (kind == 2 ? "throwcatch-" : "compute-") + row;
        }

        /** Ask the debuggee for one timing (ns per 1000 iterations). */
        static long measure(int kind) throws Exception {
            main.setValue(main.fieldByName("which"), vm.mirrorOf(kind));
            int n = ++seq;
            main.setValue(main.fieldByName("cmd"), vm.mirrorOf(n));
            long until = System.currentTimeMillis() + 300_000;
            // Polled slowly: every JDWP command republishes the gates.
            while (((IntegerValue) main.getValue(main.fieldByName("ack"))).value() != n) {
                if (System.currentTimeMillis() > until) {
                    fail("timing " + n + " never finished");
                }
                Thread.sleep(25);
            }
            return ((LongValue) main.getValue(main.fieldByName("result"))).value();
        }

        /** No request reports anything; resume whatever set arrived anyway. */
        static void drainEvents() throws Exception {
            EventSet set;
            while ((set = vm.eventQueue().remove(1)) != null) {
                for (Event e : set) {
                    if (e instanceof VMDisconnectEvent) {
                        fail("disconnected");
                    }
                    if (!(e instanceof VMStartEvent)) {
                        System.err.println("  unexpected event " + e);
                    }
                }
                set.resume();
            }
        }

        static long median(long[] xs) {
            long[] s = xs.clone();
            Arrays.sort(s);
            return s[s.length / 2];
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

/** The class the method requests name; the measured loops never call it. */
class L1W94Hot {
    static int touch() {
        return 7;
    }
}

/** A throw-free loop no request concerns. */
class L1W94Compute {
    static long mix(long x) {
        x ^= x << 13;
        x ^= x >>> 7;
        x ^= x << 17;
        return x;
    }

    static long run(int n) {
        long s = 1;
        for (int i = 0; i < n; i++) {
            s = mix(s + i);
        }
        return s;
    }
}

/** A compiled callee throws, its caller catches; no request's filter names it. */
class L1W94Thrower {
    static final IllegalStateException SHARED = new IllegalStateException("bench");

    static int fail(int i) {
        if ((i & 1) == 0) {
            throw SHARED;
        }
        return i;
    }

    static long run(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            try {
                s += fail(i);
            } catch (IllegalStateException e) {
                s ^= i;
            }
        }
        return s;
    }
}
