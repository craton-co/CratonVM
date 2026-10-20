// Interpreter round i2 wave 56, lane L6: a JDWP suspension of a spinning
// thread is complete at its reply even when the host is overloaded
// (docs/internal/fixed-bugs/interpreter-L6-a-jdwp-suspension-replies-before-its-thread-parks-on-an-overloaded-host-FIXED-20261022.md).
//
// The debuggee runs `spinner`, a Java loop that bumps a counter, and
// `2 * availableProcessors()` CPU hogs (Java loops that never block), so the
// spinner and the VM's own threads are descheduled for long slices. The
// debugger suspends the spinner over and over -- 40
// `ThreadReference.suspend()`s, then 20 `VirtualMachine.suspend()`s -- and
// each time reads its counter twice around a 20 ms pause, and its top frame.
// HotSpot's suspension is a handshake with no time bound: the reads always
// agree. CratonVM's settle wait gave up after 250 ms and replied with the
// thread still on its way (a "laggard"), so on a host at load ~45 the
// counter moved after the reply (3 of 80 repeats of
// `L1W49JdiSuspendCompletesBeforeReply`, wave 56's host run).
// And once a thread was such a laggard, no later suspension waited for it
// (the mark cleared only when a settle wait saw it stopped), so every later
// check of the section failed too. `CRATONVM_DEBUG_SUSPEND_SETTLE_LONG`
// waits up to 10 s and clears the mark whenever the thread's listing is
// published.
//
// Racy: loop it (the orchestrator's count is in the page). Canonical
// transcript: no ids, no addresses, no timings.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   ended
//
// Under a debugger:
//
//   javac -g -d out L6W56JdiSuspendUnderCpuHogs.java
//   cratonvm --java-home $JDK --jdwp-port 5794 -cp out L6W56JdiSuspendUnderCpuHogs wait
//   java -cp out L6W56JdiSuspendUnderCpuHogs debug 5794 > transcript.txt
//
// HotSpot 25.0.3 as the debuggee (default flags, the Windows box):
//
//   == attach
//     thread suspensions: 40 stable: true suspended: true top: spin
//     vm suspensions: 20 stable: true suspended: true top: spin
//   == end
//     disconnected
//
// Positive control: `CRATONVM_FRAME_TRACE=1` prints `[SUSPEND_SETTLED]
// waited=<n> laggards=0 waited_us=<n> ...`; with the switch, a `waited_us`
// above 250000 is a reply the old bound would have sent early.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;

public class L6W56JdiSuspendUnderCpuHogs {
    /** Set by the debugger: 1 = start, 3 = done. Set to 2 by the debuggee once the spinner ran. */
    static volatile int phase;
    static volatile long counter;
    static volatile long burnt;
    static volatile boolean stop;

    static void spin() {
        while (!stop) {
            counter++;
        }
    }

    /**
     * A counted inner loop of a million rounds, never blocking. (An uncounted
     * hog, polling on every back edge, did not reproduce the failure here:
     * the counted loop is what keeps the spinner's loop-exit pause waiting.)
     */
    static void hog() {
        long x = 1;
        while (!stop) {
            for (int i = 0; i < 1_000_000; i++) {
                x = x * 6364136223846793005L + 1442695040888963407L;
            }
            burnt += x & 1;
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean debugged = args.length == 1 && args[0].equals("wait");
        long until = System.currentTimeMillis() + 300_000;
        while (debugged && phase == 0 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        int hogs = 2 * Runtime.getRuntime().availableProcessors();
        List<Thread> threads = new ArrayList<>();
        for (int i = 0; i < hogs; i++) {
            Thread h = new Thread(L6W56JdiSuspendUnderCpuHogs::hog, "hog" + i);
            h.setDaemon(true);
            h.start();
            threads.add(h);
        }
        Thread spinner = new Thread(L6W56JdiSuspendUnderCpuHogs::spin, "spinner");
        spinner.setDaemon(true);
        spinner.start();
        threads.add(spinner);
        while (counter == 0) {
            Thread.sleep(1);
        }
        // Long enough for the loops to be compiled.
        Thread.sleep(500);
        if (debugged) {
            phase = 2;
        }
        while (debugged && phase != 3 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        stop = true;
        for (Thread t : threads) {
            t.join(30_000);
        }
        System.out.println("ended");
    }

    static final class Debugger {
        static VirtualMachine vm;
        static ClassType type;

        static long counter() {
            return ((LongValue) type.getValue(type.fieldByName("counter"))).value();
        }

        /** One check: the spinner's counter holds still while it is suspended. */
        static boolean stable() throws Exception {
            long first = counter();
            Thread.sleep(20);
            return first == counter();
        }

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
            type = (ClassType) awaitClass("L6W56JdiSuspendUnderCpuHogs");
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(1));
            long until = System.currentTimeMillis() + 120_000;
            IntegerValue ready;
            do {
                if (System.currentTimeMillis() > until) {
                    fail("the spinner never ran");
                }
                Thread.sleep(20);
                ready = (IntegerValue) type.getValue(type.fieldByName("phase"));
            } while (ready.value() < 2);
            ThreadReference spinner = null;
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals("spinner")) {
                    spinner = t;
                }
            }
            if (spinner == null) {
                fail("the spinner is missing");
            }
            // 1. ThreadReference.Suspend.
            long count = 0;
            boolean allStable = true;
            boolean allSuspended = true;
            Set<String> tops = new TreeSet<>();
            for (int round = 0; round < 40; round++) {
                spinner.suspend();
                count++;
                allStable &= stable();
                allSuspended &= spinner.isSuspended() && spinner.suspendCount() == 1;
                tops.add(spinner.frame(0).location().method().name());
                spinner.resume();
                Thread.sleep(5);
            }
            System.out.println("  thread suspensions: " + count + " stable: " + allStable
                    + " suspended: " + allSuspended + " top: " + String.join(",", tops));
            // 2. VirtualMachine.Suspend.
            count = 0;
            allStable = true;
            allSuspended = true;
            tops.clear();
            for (int round = 0; round < 20; round++) {
                vm.suspend();
                count++;
                allStable &= stable();
                allSuspended &= spinner.isSuspended() && spinner.suspendCount() == 1;
                tops.add(spinner.frame(0).location().method().name());
                vm.resume();
                Thread.sleep(5);
            }
            System.out.println("  vm suspensions: " + count + " stable: " + allStable
                    + " suspended: " + allSuspended + " top: " + String.join(",", tops));
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(3));
            System.out.println("== end");
            boolean ended = false;
            while (!ended) {
                EventSet next;
                try {
                    next = vm.eventQueue().remove(120_000);
                } catch (VMDisconnectedException gone) {
                    break;
                }
                if (next == null) {
                    fail("the debuggee did not end");
                }
                for (Event e : next) {
                    if (e instanceof VMDisconnectEvent) {
                        ended = true;
                    }
                }
                if (!ended) {
                    try {
                        next.resume();
                    } catch (VMDisconnectedException gone) {
                        break;
                    }
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
