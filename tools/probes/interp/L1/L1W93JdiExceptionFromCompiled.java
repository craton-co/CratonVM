// Interpreter round i4 wave 93, lane L1 -- probe and bench for JDWP
// `Exception` requests served from compiled code under the default-off
// `CRATONVM_JVMTI_EXCEPTION_FROM_COMPILED` (the wave-92 switch; page
// docs/internal/fixed-bugs/interpreter-i10-L1-proposal-exception-and-watch-events-from-compiled-code-DONE-20261224.md,
// Wave 93). Without the switch any `Exception` request keeps every method of
// the VM interpreted; with it a request that suspends nothing and filters by
// no instance (an IDE's logging exception breakpoint) lets compiled code run,
// and the compiled catch doors, the compiled `athrow`'s own site and the
// catch prediction over compiled activations report the throws.
//
// The debuggee warms `L1W93XPair` (a compiled caller catching what its
// compiled callee throws, and a rethrow), `L1W93XSelf` (a method catching its
// own throw) and the throw-free `L1W93Compute` until they are compiled. The
// debugger sets one `ExceptionRequest` (caught and uncaught, `SUSPEND_NONE`,
// class filter `L1W93X*`, so the JDK's own exceptions are not reported) and
// the debuggee times `L1W93Compute` and a throw-and-catch loop of a class the
// filter does not name, then throws once in each scenario and ends with an
// `IndexOutOfBoundsException`, the debugger's sentinel.
//
// Plain run (no debugger): stdout must equal HotSpot 25's: `ended`.
//
// Under a debugger (debug build, `experimental-debug`):
//
//   javac -g -d out L1W93JdiExceptionFromCompiled.java
//   cratonvm [--compatible] --java-home $JDK --jdwp-port 5894 -cp out L1W93JdiExceptionFromCompiled wait
//   java -cp out L1W93JdiExceptionFromCompiled debug 5894
//   (A/B: the debuggee with CRATONVM_JVMTI_EXCEPTION_FROM_COMPILED unset, then
//    =1. HotSpot as the debuggee:
//    java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5894 -cp out L1W93JdiExceptionFromCompiled wait)
//
// Debugger stdout; the `bench` line is the measurement (median ns per 1000
// iterations of the throw-free loop before the request and while it stands,
// and of the throw-and-catch loop while it stands: unset, `compute during`
// is the interpreter's speed; with the switch, about `compute before`). It is
// excluded from the comparison; the rest is identical in every arm. Lines are
// this file's:
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3 debuggee, derived from the source)
// == attach
// == request
//   java.lang.IllegalStateException at L1W93XPair.callee:280 caught at L1W93XPair.caller:275
//   java.lang.ArithmeticException at L1W93XSelf.local:308 caught at L1W93XSelf.local:311
//   java.lang.IllegalStateException at L1W93XPair.callee:280 caught at L1W93XPair.rethrow:288
//   java.lang.UnsupportedOperationException at L1W93XPair.rethrow:289 caught at L1W93JdiExceptionFromCompiled.main:96
//   java.lang.IndexOutOfBoundsException at L1W93XPair.end:296 caught at L1W93XPair.end:297
//   events: 5
// == end
//   disconnected
// HOTSPOT_EXPECTED_END
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W93JdiExceptionFromCompiled {
    /** Set by the debugger: 1 = the request stands, 3 = done. */
    static volatile int phase;
    /** Set by the debuggee once its classes are warm. */
    static volatile boolean warm;
    static volatile long computeNsBefore;
    static volatile long computeNsDuring;
    static volatile long throwNsDuring;
    static volatile long sink;

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean debugged = args.length == 1 && args[0].equals("wait");
        long until = System.currentTimeMillis() + 300_000;
        for (int round = 0; round < 40; round++) {
            sink += L1W93Compute.run(200_000);
            sink += L1W93BenchThrow.run(2_000);
            for (int i = 0; i < 2_000; i++) {
                sink += L1W93XPair.caller(i);
                sink += L1W93XSelf.local(i);
                try {
                    sink += L1W93XPair.rethrow(i);
                } catch (UnsupportedOperationException e) {
                    sink++;
                }
            }
        }
        computeNsBefore = timeCompute();
        warm = true;
        while (debugged && phase != 1 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        computeNsDuring = timeCompute();
        throwNsDuring = timeThrow();
        sink += L1W93XPair.caller(1);
        sink += L1W93XSelf.local(1);
        try {
            sink += L1W93XPair.rethrow(1);
        } catch (UnsupportedOperationException e) {
            sink++;
        }
        L1W93XPair.end();
        while (debugged && phase != 3 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        System.out.println("ended");
    }

    /** Median nanoseconds per 1000 iterations of `L1W93Compute.run`. */
    static long timeCompute() {
        final int n = 2_000_000;
        long[] ns = new long[7];
        for (int i = 0; i < ns.length; i++) {
            long t0 = System.nanoTime();
            sink += L1W93Compute.run(n);
            ns[i] = (System.nanoTime() - t0) / (n / 1000);
        }
        Arrays.sort(ns);
        return ns[ns.length / 2];
    }

    /** Median nanoseconds per 1000 throw-and-catch iterations. */
    static long timeThrow() {
        final int n = 20_000;
        long[] ns = new long[5];
        for (int i = 0; i < ns.length; i++) {
            long t0 = System.nanoTime();
            sink += L1W93BenchThrow.run(n);
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
            ClassType main = (ClassType) awaitClass("L1W93JdiExceptionFromCompiled");
            long until = System.currentTimeMillis() + 240_000;
            while (!((BooleanValue) main.getValue(main.fieldByName("warm"))).value()) {
                if (System.currentTimeMillis() > until) {
                    fail("the debuggee never warmed up");
                }
                Thread.sleep(20);
            }
            EventRequestManager erm = vm.eventRequestManager();
            ExceptionRequest request = erm.createExceptionRequest(null, true, true);
            request.addClassFilter("L1W93X*");
            request.setSuspendPolicy(EventRequest.SUSPEND_NONE);
            request.enable();
            System.out.println("== request");
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
                    if (e instanceof ExceptionEvent x) {
                        count++;
                        String type = x.exception().referenceType().name();
                        System.out.println("  " + type + " at " + where(x.location())
                                + " caught at " + where(x.catchLocation()));
                        ended = type.equals("java.lang.IndexOutOfBoundsException");
                    } else if (e instanceof VMDisconnectEvent) {
                        fail("disconnected after " + count);
                    }
                }
                set.resume();
            }
            System.out.println("  events: " + count);
            erm.deleteEventRequest(request);
            long before = ((LongValue) main.getValue(main.fieldByName("computeNsBefore"))).value();
            long during = ((LongValue) main.getValue(main.fieldByName("computeNsDuring"))).value();
            long throwing = ((LongValue) main.getValue(main.fieldByName("throwNsDuring"))).value();
            System.out.println("  bench ns/1000 compute before " + before + " during " + during
                    + " throwcatch during " + throwing);
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

        static String where(Location at) {
            if (at == null) {
                return "<uncaught>";
            }
            return at.declaringType().name() + "." + at.method().name() + ":" + at.lineNumber();
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

/** A compiled caller catching its compiled callee's throw, and a rethrow. */
class L1W93XPair {
    static int caller(int x) {
        try {
            return callee(x);
        } catch (IllegalStateException e) { return -1; }
    }

    static int callee(int x) {
        if (x >= 0) {
            throw new IllegalStateException("x");
        }
        return x;
    }

    static int rethrow(int x) {
        try {
            return callee(x);
        } catch (IllegalStateException e) {
            throw new UnsupportedOperationException(e);
        }
    }

    static void end() {
        try {
            L1W93JdiExceptionFromCompiled.sink++;
            throw new IndexOutOfBoundsException("end");
        } catch (IndexOutOfBoundsException e) {
            L1W93JdiExceptionFromCompiled.sink--;
        }
    }
}

/** A compiled method catching its own throw. */
class L1W93XSelf {
    static int local(int x) {
        try {
            if (x >= 0) {
                throw new ArithmeticException("x");
            }
            return x;
        } catch (ArithmeticException e) {
            return -2;
        }
    }
}

/** A throw-free loop: it should stay compiled while the request stands. */
class L1W93Compute {
    static long run(int n) {
        long s = 1;
        for (int i = 0; i < n; i++) {
            s = s * 31 + (i ^ (s >>> 7));
        }
        return s;
    }
}

/** A throw-and-catch loop of a class the request's filter does not name. */
class L1W93BenchThrow {
    static long run(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            try {
                s += thrower(i);
            } catch (IllegalArgumentException e) {
                s--;
            }
        }
        return s;
    }

    static int thrower(int i) {
        if (i >= 0) {
            throw new IllegalArgumentException();
        }
        return i;
    }
}
