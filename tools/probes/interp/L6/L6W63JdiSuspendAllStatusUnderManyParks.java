// Interpreter round i2 wave 63, lane L6: a `VirtualMachine.suspend()` over N
// spinning threads (default 64; `debug <port> <n>` and `wait <n>` change it),
// held while the debugger asks 1 000 `ThreadReference.status()` /
// `isSuspended()` questions -- the measurement scenario of
// docs/internal/fixed-bugs/interpreter-i54-L6-proposal-a-per-thread-suspend-word-instead-of-the-debug-state-lock-DONE-20261104.md
// (`CRATONVM_DEBUG_THREAD_SUSPEND_WORD`, default off). With the switch off,
// every parked thread takes the VM-wide debug-state lock once per 1 ms poll
// (N lock rounds a millisecond against the lock every JDWP command takes);
// with it on, a parked thread waits on its own suspend word and takes the
// lock only when its word moves.
//
// Stdout is behavioural and must equal HotSpot 25's; the timing rows go to
// stderr (`ns/op` of one status question, and of one suspend + resume of the
// whole VM), to be compared switch on / off on the host and against HotSpot.
//
// Plain run (no debugger): stdout is `ended`.
//
// Under a debugger:
//
//   javac -g -d out L6W63JdiSuspendAllStatusUnderManyParks.java
//   cratonvm --java-home $JDK --jdwp-port 5801 -cp out L6W63JdiSuspendAllStatusUnderManyParks wait
//   java -cp out L6W63JdiSuspendAllStatusUnderManyParks debug 5801 > transcript.txt 2> timings.txt
//   (and the same with CRATONVM_DEBUG_THREAD_SUSPEND_WORD=1 on the debuggee)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// == attach
//   spinners: 64
//   suspended: 64 running: 64
//   questions: 1000 all suspended: true
//   held still while suspended: true
//   round trips: 50 held still each time: true
//   ran after the resume: true
// == end
//   disconnected
// HOTSPOT_EXPECTED_END
//
// The debuggee's own stdout is `ended` in both runs. With the switch on,
// `CRATONVM_FRAME_TRACE=1` prints one
// `[DEBUG_PARK_WORD] tid=<n> rounds=<n> locked=<n> word_wakes=<n>` per park
// (beside `[DEBUG_PARK]`): `locked` is a handful (the first round, one per
// 50 ms backstop, the round the resume ends) where `rounds` counts every
// 1 ms safepoint poll -- the positive control that the parks left the lock.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;

public class L6W63JdiSuspendAllStatusUnderManyParks {
    static final int QUESTIONS = 1_000;
    static final int ROUND_TRIPS = 50;
    /** Set by the debugger: 1 = start, 3 = done. */
    static volatile int phase;
    /** Spinners started so far. */
    static volatile int started;
    /** One counter per spinner. */
    static long[] counts;

    static void spin(int slot) {
        synchronized (L6W63JdiSuspendAllStatusUnderManyParks.class) {
            started++;
        }
        long[] mine = counts;
        while (phase != 3) {
            mine[slot]++;
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length >= 2 && args[0].equals("debug")) {
            int n = args.length >= 3 ? Integer.parseInt(args[2]) : 64;
            Debugger.run(Integer.parseInt(args[1]), n);
            return;
        }
        boolean debugged = args.length >= 1 && args[0].equals("wait");
        int n = args.length >= 2 ? Integer.parseInt(args[1]) : 64;
        counts = new long[n];
        long until = System.currentTimeMillis() + 300_000;
        while (debugged && phase == 0 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        if (!debugged) {
            phase = 3;
        }
        Thread[] spinners = new Thread[n];
        for (int i = 0; i < n; i++) {
            int slot = i;
            spinners[i] = new Thread(() -> spin(slot), "spinner-" + i);
            spinners[i].setDaemon(true);
            spinners[i].start();
        }
        while (debugged && phase != 3 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        for (Thread t : spinners) {
            t.join(10_000);
        }
        System.out.println("ended");
    }

    static final class Debugger {
        static VirtualMachine vm;
        static ClassType type;

        static int intField(String name) {
            return ((IntegerValue) type.getValue(type.fieldByName(name))).value();
        }

        static long sum() {
            ArrayReference a = (ArrayReference) type.getValue(type.fieldByName("counts"));
            long s = 0;
            for (Value v : a.getValues()) {
                s += ((LongValue) v).value();
            }
            return s;
        }

        static void run(int port, int n) throws Exception {
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
            type = (ClassType) awaitClass("L6W63JdiSuspendAllStatusUnderManyParks");
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(1));
            long until = System.currentTimeMillis() + 120_000;
            while (intField("started") < n) {
                if (System.currentTimeMillis() > until) {
                    fail("only " + intField("started") + " spinners started");
                }
                Thread.sleep(10);
            }
            List<ThreadReference> spinners = new ArrayList<>();
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().startsWith("spinner-")) {
                    spinners.add(t);
                }
            }
            System.out.println("  spinners: " + spinners.size());

            vm.suspend();
            int suspended = 0;
            int running = 0;
            for (ThreadReference t : spinners) {
                if (t.isSuspended()) {
                    suspended++;
                }
                if (t.status() == ThreadReference.THREAD_STATUS_RUNNING) {
                    running++;
                }
            }
            System.out.println("  suspended: " + suspended + " running: " + running);

            boolean allSuspended = true;
            long start = System.nanoTime();
            for (int i = 0; i < QUESTIONS; i++) {
                ThreadReference t = spinners.get(i % spinners.size());
                t.status();
                allSuspended &= t.isSuspended();
            }
            long questionNs = (System.nanoTime() - start) / QUESTIONS;
            System.out.println("  questions: " + QUESTIONS + " all suspended: " + allSuspended);

            long before = sum();
            Thread.sleep(100);
            System.out.println("  held still while suspended: " + (sum() == before));
            vm.resume();

            // VM-wide round trips: each suspension stops every spinner, each
            // resume lets them all go (with the switch on: one word write and
            // one unpark per parked spinner).
            boolean stillEachTime = true;
            start = System.nanoTime();
            for (int i = 0; i < ROUND_TRIPS; i++) {
                vm.suspend();
                long s = sum();
                stillEachTime &= sum() == s;
                vm.resume();
            }
            long roundTripNs = (System.nanoTime() - start) / ROUND_TRIPS;
            System.out.println("  round trips: " + ROUND_TRIPS + " held still each time: " + stillEachTime);

            long after = sum();
            until = System.currentTimeMillis() + 10_000;
            while (sum() == after && System.currentTimeMillis() < until) {
                Thread.sleep(1);
            }
            System.out.println("  ran after the resume: " + (sum() != after));
            System.err.println("status question (" + spinners.size() + " parked): " + questionNs + " ns/op");
            System.err.println("vm suspend+resume (" + spinners.size() + " spinners): " + roundTripNs + " ns/op");

            type.setValue(type.fieldByName("phase"), vm.mirrorOf(3));
            System.out.println("== end");
            boolean ended = false;
            while (!ended) {
                EventSet next;
                try {
                    next = vm.eventQueue().remove(60_000);
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
