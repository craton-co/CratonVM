// Interpreter round i4 wave 93, lane L1 -- probe and bench for the default-off
// `CRATONVM_JDWP_SCOPED_METHOD_EVENTS` (stage 1 of
// docs/internal/fixed-bugs/interpreter-i39-L1-proposal-scope-class-filtered-method-events-to-their-classes-DONE-20261127.md):
// an IDE's method breakpoint is a JDWP `MethodEntry` / `MethodExit` request
// filtered to the method's class. Without the switch such a request keeps
// every method of the VM interpreted and withdraws every compiled body; with
// it only the filtered class's methods (and the compiled code that inlined
// them) leave compiled code.
//
// The debuggee warms three classes until they are compiled: `L1W93Hot` (the
// class the requests name), `L1W93Driver` (a loop that calls -- and in
// compiled code inlines -- `L1W93Hot.step`) and `L1W93Cold` (a throw-free
// loop no request concerns). The debugger then sets a `MethodEntry` request
// filtered by `ClassOnly` (a `ReferenceType` filter) and a `MethodExit`
// request filtered by an exact `ClassMatch` (a name filter), both on
// `L1W93Hot`, `SUSPEND_NONE`, and the debuggee runs `L1W93Hot.burst()`,
// `L1W93Driver.drive(2)`, a timed `L1W93Cold` loop and `L1W93Hot.end()`. The
// debugger prints every event up to `end`'s entry.
//
// Plain run (no debugger): stdout must equal HotSpot 25's: `ended`.
//
// Under a debugger (debug build, `experimental-debug`):
//
//   javac -g -d out L1W93JdiScopedMethodEvents.java
//   cratonvm [--compatible] --java-home $JDK --jdwp-port 5893 -cp out L1W93JdiScopedMethodEvents wait
//   java -cp out L1W93JdiScopedMethodEvents debug 5893
//   (A/B: the debuggee with CRATONVM_JDWP_SCOPED_METHOD_EVENTS unset, then =1;
//    positive control with CRATONVM_DBG_JITC=1 and the switch: one
//    `[cratonvm-jitc] method event scope: scopes=2 classes=1 ...` line, one
//    `[cratonvm-jitc] breakpoint withdrawal: class=L1W93Hot ... scoped=true`
//    line and NO `interpreter-only withdrawal: source=jdwp` line; without the
//    switch the latter appears. HotSpot as the debuggee:
//    java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5893 -cp out L1W93JdiScopedMethodEvents wait)
//
// Debugger stdout; the `bench` line is the measurement (median ns per
// `L1W93Cold.mix` call before the requests and while they stand: unset, the
// second is the interpreter's speed; with the switch, about the first). It is
// excluded from the comparison; the rest is identical in every arm:
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3 debuggee, derived from the source)
// == attach
// == requests
//   entry L1W93Hot.burst()I
//   entry L1W93Hot.step(I)I
//   exit L1W93Hot.step(I)I
//   entry L1W93Hot.step(I)I
//   exit L1W93Hot.step(I)I
//   entry L1W93Hot.<init>()V
//   exit L1W93Hot.<init>()V
//   entry L1W93Hot.twice(I)I
//   entry L1W93Hot.step(I)I
//   exit L1W93Hot.step(I)I
//   exit L1W93Hot.twice(I)I
//   exit L1W93Hot.burst()I
//   entry L1W93Hot.step(I)I
//   exit L1W93Hot.step(I)I
//   entry L1W93Hot.step(I)I
//   exit L1W93Hot.step(I)I
//   entry L1W93Hot.end()V
//   events: 17
// == end
//   disconnected
// HOTSPOT_EXPECTED_END
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W93JdiScopedMethodEvents {
    /** Set by the debugger: 1 = the requests stand, 3 = done. */
    static volatile int phase;
    /** Set by the debuggee once the three classes are warm. */
    static volatile boolean warm;
    static volatile long coldNsBefore;
    static volatile long coldNsDuring;
    static volatile long sink;

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean debugged = args.length == 1 && args[0].equals("wait");
        long until = System.currentTimeMillis() + 300_000;
        for (int round = 0; round < 40; round++) {
            sink += L1W93Cold.run(200_000);
            sink += L1W93Driver.drive(100_000);
            sink += L1W93Hot.warmTwice(50_000);
        }
        coldNsBefore = timeCold();
        warm = true;
        while (debugged && phase != 1 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        sink += L1W93Hot.burst();
        sink += L1W93Driver.drive(2);
        coldNsDuring = timeCold();
        L1W93Hot.end();
        while (debugged && phase != 3 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        System.out.println("ended");
    }

    /** Median nanoseconds per `L1W93Cold.mix` call over seven runs. */
    static long timeCold() {
        final int n = 2_000_000;
        long[] ns = new long[7];
        for (int i = 0; i < ns.length; i++) {
            long t0 = System.nanoTime();
            sink += L1W93Cold.run(n);
            ns[i] = (System.nanoTime() - t0) / (n / 1000);
        }
        Arrays.sort(ns);
        return ns[ns.length / 2];
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
            System.out.println("== attach");
            ClassType main = (ClassType) awaitClass("L1W93JdiScopedMethodEvents");
            long until = System.currentTimeMillis() + 240_000;
            while (!((BooleanValue) main.getValue(main.fieldByName("warm"))).value()) {
                if (System.currentTimeMillis() > until) {
                    fail("the debuggee never warmed up");
                }
                Thread.sleep(20);
            }
            ReferenceType hot = awaitClass("L1W93Hot");
            EventRequestManager erm = vm.eventRequestManager();
            MethodEntryRequest entries = erm.createMethodEntryRequest();
            entries.addClassFilter(hot);
            entries.setSuspendPolicy(EventRequest.SUSPEND_NONE);
            entries.enable();
            MethodExitRequest exits = erm.createMethodExitRequest();
            exits.addClassFilter("L1W93Hot");
            exits.setSuspendPolicy(EventRequest.SUSPEND_NONE);
            exits.enable();
            System.out.println("== requests");
            main.setValue(main.fieldByName("phase"), vm.mirrorOf(1));
            int count = 0;
            boolean ended = false;
            while (!ended) {
                EventSet set = vm.eventQueue().remove(120_000);
                if (set == null) {
                    fail("no event after " + count);
                }
                for (Event e : set) {
                    if (ended) {
                        break;
                    }
                    if (e instanceof MethodEntryEvent me) {
                        count++;
                        System.out.println("  entry " + name(me.method()));
                        ended = me.method().name().equals("end");
                    } else if (e instanceof MethodExitEvent mx) {
                        count++;
                        System.out.println("  exit " + name(mx.method()));
                    } else if (e instanceof VMDisconnectEvent) {
                        fail("disconnected after " + count);
                    }
                }
                set.resume();
            }
            System.out.println("  events: " + count);
            erm.deleteEventRequest(entries);
            erm.deleteEventRequest(exits);
            long before = ((LongValue) main.getValue(main.fieldByName("coldNsBefore"))).value();
            long during = ((LongValue) main.getValue(main.fieldByName("coldNsDuring"))).value();
            System.out.println("  bench cold ns/1000calls before " + before + " during " + during);
            main.setValue(main.fieldByName("phase"), vm.mirrorOf(3));
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

        static String name(Method m) {
            return m.declaringType().name() + "." + m.name() + m.signature();
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

/** The class the requests name. */
class L1W93Hot {
    static int step(int x) {
        return x * 3 + 1;
    }

    int twice(int x) {
        return step(x) * 2;
    }

    static int burst() {
        int a = step(1);
        int b = step(2);
        L1W93Hot h = new L1W93Hot();
        int c = h.twice(3);
        long d = L1W93Cold.mix(a + b + c);
        return (int) d;
    }

    static long warmTwice(int n) {
        L1W93Hot h = new L1W93Hot();
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += h.twice(i);
        }
        return s;
    }

    static void end() {
        L1W93JdiScopedMethodEvents.sink++;
    }
}

/** A loop of another class whose compiled body inlines `L1W93Hot.step`. */
class L1W93Driver {
    static long drive(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += L1W93Hot.step(i);
        }
        return s;
    }
}

/** A loop no request concerns: it should stay compiled. */
class L1W93Cold {
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
