// Interpreter round i4 wave 95, lane L1 -- JDWP field watches from compiled
// code, stage 1 (CRATONVM_JDWP_SCOPED_FIELD_WATCHES, default OFF; page
// docs/internal/fixed-bugs/interpreter-i10-L1-proposal-exception-and-watch-events-from-compiled-code-DONE-20261224.md).
//
// A field watch kept every method of the VM interpreted and withdrew every
// compiled body; with the switch only the classes whose constant pools name
// the watched field (`L1W95Accessor` here) stay interpreted and only their
// dependents are withdrawn.
//
// Two debugger scenarios:
//
//   events  correctness. `L1W95Caller.callBump` (compiled, out of scope) calls
//           `L1W95Accessor.bump` (compiled before the watch, a baked call)
//           three times after SUSPEND_NONE access and modification watches on
//           `L1W95Watched.value` arm; every access and modification must be
//           reported, at `bump`'s line, as HotSpot reports them.
//   bench   the A/B. Times, per rep, `L1W95Compute.run` (names no watched
//           field) and `L1W95Neighbor.run` (writes `L1W95Watched.other`,
//           a neighbour field of the watched one) before the watches, while
//           they stand, and twice after. Unset: `*-during` at the
//           interpreter's speed; =1: near `*-before`. `arm-us` is the wall
//           time of arming both watches.
//
// Run (debug build, `--features experimental-debug`):
//
//   javac -g -d out L1W95JdiScopedFieldWatch.java
//   CRATONVM_JDWP_SCOPED_FIELD_WATCHES=1 cratonvm [--compatible] --java-home $JDK --jdwp-port 5900 -cp out L1W95JdiScopedFieldWatch wait
//   java -cp out L1W95JdiScopedFieldWatch debug 5900 events      (then again with `bench`)
//   # HotSpot debuggee (the reference):
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5900 -cp out L1W95JdiScopedFieldWatch wait
//
// Each scenario needs a fresh debuggee. Positive control: add
// CRATONVM_DBG_JITC=1 -- with the switch one `[cratonvm-jitc] field watch
// scope: fields=1 classes=1` line when the watches arm, and no
// `interpreter-only withdrawal: source=jdwp` line.
//
// Plain run (no debugger): stdout must equal HotSpot 25's: `ended`.
// Debugger stdout, `events` (the `bench` scenario prints `== bench`,
// `  reps: 5`, `== end`, `  disconnected`; its `bench` line goes to stderr):
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3 debuggee, derived from the source)
// == attach
// == events
//   access L1W95Accessor.bump line 352
//   modification L1W95Accessor.bump line 352
//   access L1W95Accessor.bump line 352
//   modification L1W95Accessor.bump line 352
//   access L1W95Accessor.bump line 352
//   modification L1W95Accessor.bump line 352
//   events 6 value 3
// == end
//   disconnected
// HOTSPOT_EXPECTED_END
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W95JdiScopedFieldWatch {
    /** The debugger's request number; -1 ends the debuggee. */
    static volatile int cmd;
    /** The last request number served. */
    static volatile int ack;
    /** What to run: 1 = three bumps, 2 = time compute, 3 = time neighbour. */
    static volatile int which;
    /** The last timing, ns per 1000 iterations, or the bumped value. */
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
        L1W95Watched warmUp = new L1W95Watched();
        for (int round = 0; round < 40; round++) {
            sink += L1W95Caller.callBump(warmUp, 20_000);
            sink += L1W95Compute.run(200_000);
            sink += L1W95Neighbor.run(warmUp, 200_000);
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
            if (which == 1) {
                result = L1W95Caller.callBump(new L1W95Watched(), 3);
            } else {
                result = time(which);
            }
            ack = c;
        }
        System.out.println("ended");
    }

    static long time(int kind) {
        final int n = 1_000_000;
        L1W95Watched w = new L1W95Watched();
        long[] ns = new long[5];
        for (int i = 0; i < ns.length; i++) {
            long t0 = System.nanoTime();
            sink += kind == 3 ? L1W95Neighbor.run(w, n) : L1W95Compute.run(n);
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
            main = (ClassType) awaitClass("L1W95JdiScopedFieldWatch");
            long until = System.currentTimeMillis() + 300_000;
            while (!((BooleanValue) main.getValue(main.fieldByName("warm"))).value()) {
                if (System.currentTimeMillis() > until) {
                    fail("the debuggee never warmed up");
                }
                Thread.sleep(20);
            }
            ReferenceType watched = awaitClass("L1W95Watched");
            Field value = watched.fieldByName("value");
            EventRequestManager erm = vm.eventRequestManager();
            AccessWatchpointRequest reads = erm.createAccessWatchpointRequest(value);
            reads.setSuspendPolicy(EventRequest.SUSPEND_NONE);
            ModificationWatchpointRequest writes = erm.createModificationWatchpointRequest(value);
            writes.setSuspendPolicy(EventRequest.SUSPEND_NONE);
            List<EventRequest> requests = List.of(reads, writes);
            System.out.println("== " + scenario);
            if (scenario.equals("events")) {
                events(requests);
            } else if (scenario.equals("bench")) {
                bench(requests);
            } else {
                fail("unknown scenario " + scenario);
            }
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

        /** Arm the watches, run three bumps, print every event they sent. */
        static void events(List<EventRequest> requests) throws Exception {
            for (EventRequest r : requests) {
                r.enable();
            }
            long bumped = request(1);
            List<String> seen = new ArrayList<>();
            long until = System.currentTimeMillis() + 10_000;
            while (seen.size() < 6 && System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(500);
                if (set == null) {
                    continue;
                }
                for (Event e : set) {
                    if (e instanceof WatchpointEvent w) {
                        Location at = w.location();
                        seen.add((e instanceof AccessWatchpointEvent ? "access " : "modification ")
                                + at.declaringType().name() + "." + at.method().name()
                                + " line " + at.lineNumber());
                    } else if (e instanceof VMDisconnectEvent) {
                        fail("disconnected");
                    }
                }
                set.resume();
            }
            for (EventRequest r : requests) {
                r.disable();
            }
            for (String s : seen) {
                System.out.println("  " + s);
            }
            System.out.println("  events " + seen.size() + " value " + bumped);
        }

        /** The A/B timings; the `bench` line goes to stderr. */
        static void bench(List<EventRequest> requests) throws Exception {
            final int reps = 5;
            int[] kinds = {2, 3};
            String[] rows = {"before", "during", "after1", "after2"};
            Map<String, long[]> samples = new LinkedHashMap<>();
            long[] armUs = new long[reps];
            for (int rep = 0; rep < reps; rep++) {
                for (int kind : kinds) {
                    samples.computeIfAbsent(label(kind, "before"), k -> new long[reps])[rep] = request(kind);
                }
                long t0 = System.nanoTime();
                for (EventRequest r : requests) {
                    r.enable();
                }
                armUs[rep] = (System.nanoTime() - t0) / 1000;
                for (int kind : kinds) {
                    samples.computeIfAbsent(label(kind, "during"), k -> new long[reps])[rep] = request(kind);
                }
                for (EventRequest r : requests) {
                    r.disable();
                }
                for (String row : new String[] {"after1", "after2"}) {
                    for (int kind : kinds) {
                        samples.computeIfAbsent(label(kind, row), k -> new long[reps])[rep] = request(kind);
                    }
                }
                EventSet set;
                while ((set = vm.eventQueue().remove(1)) != null) {
                    for (Event e : set) {
                        if (!(e instanceof VMStartEvent)) {
                            System.err.println("  unexpected event " + e);
                        }
                    }
                    set.resume();
                }
            }
            System.out.println("  reps: " + reps);
            StringBuilder line = new StringBuilder("  bench watches");
            for (int kind : kinds) {
                for (String row : rows) {
                    line.append(' ').append(label(kind, row)).append(' ')
                            .append(median(samples.get(label(kind, row))));
                }
            }
            line.append(" arm-us ").append(median(armUs));
            // A `bench` line: excluded from the comparison.
            System.err.println(line);
        }

        static String label(int kind, String row) {
            return (kind == 3 ? "neighbor-" : "compute-") + row;
        }

        /** Ask the debuggee for one run; its result. */
        static long request(int kind) throws Exception {
            main.setValue(main.fieldByName("which"), vm.mirrorOf(kind));
            int n = ++seq;
            main.setValue(main.fieldByName("cmd"), vm.mirrorOf(n));
            long until = System.currentTimeMillis() + 300_000;
            // Polled slowly: every JDWP command republishes the gates.
            while (((IntegerValue) main.getValue(main.fieldByName("ack"))).value() != n) {
                if (System.currentTimeMillis() > until) {
                    fail("request " + n + " never finished");
                }
                Thread.sleep(25);
            }
            return ((LongValue) main.getValue(main.fieldByName("result"))).value();
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

/** The watched field and its neighbour. */
class L1W95Watched {
    int value;
    int other;
}

/** The only class whose constant pool names `L1W95Watched.value`. */
class L1W95Accessor {
    static int bump(L1W95Watched w) {
        return w.value++;
    }
}

/** Compiled, out of the watch's scope: calls the accessor. */
class L1W95Caller {
    static long callBump(L1W95Watched w, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += L1W95Accessor.bump(w);
        }
        return s;
    }
}

/** A loop no watch concerns. */
class L1W95Compute {
    static long run(int n) {
        long s = 1;
        for (int i = 0; i < n; i++) {
            s ^= s << 13;
            s ^= s >>> 7;
            s += i;
        }
        return s;
    }
}

/** Writes the watched field's neighbour; names no watched field. */
class L1W95Neighbor {
    static long run(L1W95Watched w, int n) {
        for (int i = 0; i < n; i++) {
            w.other += i;
        }
        return w.other;
    }
}
