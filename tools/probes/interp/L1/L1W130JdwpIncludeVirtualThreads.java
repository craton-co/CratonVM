// Interpreter round i6 wave 130, lane L1b -- `-agentlib:jdwp=...,includevirtualthreads=y`.
// Page: docs/known-issues/interpreter/i130-L1b-agentlib-includevirtualthreads-dropped-20261229.md.
//
// The launcher accepted `includevirtualthreads=y` and dropped it, so
// `VirtualMachine.AllThreads` left the virtual threads out where HotSpot's
// back end lists them (only `CRATONVM_JDWP_INCLUDE_VIRTUAL_THREADS=1` turned
// them on). The debuggee starts a virtual thread `vt-before` before the
// debugger attaches and, on the debugger's word, `vt-after`; both park on a
// latch. The debugger lists `allThreads()` after each start and prints which
// `vt-*` threads it sees (and whether JDI calls them virtual), and whether
// `main` is listed. Thread counts are not printed: the VMs' system threads
// differ.
//
//   javac -g -d out L1W130JdwpIncludeVirtualThreads.java
//   Debuggee, A (HotSpot's default, no virtual threads listed):
//     cratonvm [--compatible] --java-home $JDK -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5895 -cp out L1W130JdwpIncludeVirtualThreads wait
//   Debuggee, B (virtual threads listed):
//     cratonvm [--compatible] --java-home $JDK -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5895,includevirtualthreads=y -cp out L1W130JdwpIncludeVirtualThreads wait
//   Debugger (HotSpot), for either:
//     java -cp out L1W130JdwpIncludeVirtualThreads debug 5895
//   (HotSpot as the debuggee: the same `-agentlib:jdwp=...` line on `java`.
//    Negative control for B: CRATONVM_JDWP_AGENT_INCLUDE_VIRTUAL_THREADS=0
//    prints arm A's block.)
//
// Plain run (no debugger): stdout must equal HotSpot 25's: `ended`.
//
// Debugger stdout, arm A (no option):
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3 debuggee, measured, both default flags and -Xint)
// == attach
//   listed: main=true vt-before=false vt-after=false
// == after start
//   listed: main=true vt-before=false vt-after=false
// == end
//   disconnected
// HOTSPOT_EXPECTED_END
//
// Debugger stdout, arm B (`includevirtualthreads=y`):
// HOTSPOT_EXPECTED_B_BEGIN (JDK 25.0.3 debuggee, measured, both default flags and -Xint)
// == attach
//   listed: main=true vt-before=true(virtual) vt-after=false
// == after start
//   listed: main=true vt-before=true(virtual) vt-after=true(virtual)
// == end
//   disconnected
// HOTSPOT_EXPECTED_B_END
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import java.util.*;
import java.util.concurrent.CountDownLatch;

public class L1W130JdwpIncludeVirtualThreads {
    /** Set by the debugger: 1 = start `vt-after`, 3 = done. */
    static volatile int phase;
    /** Set by the debuggee: 1 = `vt-before` parked, 2 = `vt-after` parked too. */
    static volatile int ready;
    static final CountDownLatch release = new CountDownLatch(1);
    static final CountDownLatch parked = new CountDownLatch(2);

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean debugged = args.length == 1 && args[0].equals("wait");
        long until = System.currentTimeMillis() + 240_000;
        Runnable park = () -> {
            parked.countDown();
            try {
                release.await();
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        };
        Thread before = Thread.ofVirtual().name("vt-before").start(park);
        while (parked.getCount() > 1) {
            Thread.sleep(5);
        }
        ready = 1;
        while (debugged && phase != 1 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        Thread after = Thread.ofVirtual().name("vt-after").start(park);
        parked.await();
        // Both are parked on the latch (unmounted) once their state says so.
        while (after.getState() != Thread.State.WAITING || before.getState() != Thread.State.WAITING) {
            Thread.sleep(5);
        }
        ready = 2;
        while (debugged && phase != 3 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        release.countDown();
        before.join();
        after.join();
        System.out.println("ended");
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
            ClassType main = (ClassType) awaitClass("L1W130JdwpIncludeVirtualThreads");
            awaitReady(main, 1);
            System.out.println("  listed: " + listing());
            main.setValue(main.fieldByName("phase"), vm.mirrorOf(1));
            awaitReady(main, 2);
            System.out.println("== after start");
            System.out.println("  listed: " + listing());
            main.setValue(main.fieldByName("phase"), vm.mirrorOf(3));
            System.out.println("== end");
            boolean gone = false;
            while (!gone) {
                com.sun.jdi.event.EventSet next;
                try {
                    next = vm.eventQueue().remove(60_000);
                } catch (VMDisconnectedException disconnected) {
                    break;
                }
                if (next == null) {
                    fail("the debuggee did not end");
                }
                for (com.sun.jdi.event.Event e : next) {
                    if (e instanceof com.sun.jdi.event.VMDisconnectEvent) {
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

        static String listing() {
            Map<String, String> seen = new TreeMap<>();
            for (ThreadReference t : vm.allThreads()) {
                String name;
                try {
                    name = t.name();
                } catch (ObjectCollectedException gone) {
                    continue;
                }
                if (name.equals("main") || name.startsWith("vt-")) {
                    seen.put(name, t.isVirtual() ? "true(virtual)" : "true");
                }
            }
            return "main=" + seen.getOrDefault("main", "false")
                    + " vt-before=" + seen.getOrDefault("vt-before", "false")
                    + " vt-after=" + seen.getOrDefault("vt-after", "false");
        }

        static void awaitReady(ClassType main, int value) throws Exception {
            long until = System.currentTimeMillis() + 120_000;
            while (((IntegerValue) main.getValue(main.fieldByName("ready"))).value() < value) {
                if (System.currentTimeMillis() > until) {
                    fail("the debuggee never reached ready=" + value);
                }
                Thread.sleep(20);
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
