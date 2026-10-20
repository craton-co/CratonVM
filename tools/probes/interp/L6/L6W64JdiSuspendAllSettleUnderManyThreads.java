// Interpreter round i2 wave 64, lane L6: 100 `VirtualMachine.suspend()` /
// `resume()` round trips over N spinning threads (default 256; `debug <port>
// <n>` and `wait <n>` change it), each suspension checked complete when its
// reply arrives (every spinner reads suspended and holds still) -- the
// measurement scenario of
// docs/internal/fixed-bugs/interpreter-i63-L6-proposal-arrival-bits-in-the-suspend-word-KEPT-OFF-20261019.md
// (`CRATONVM_DEBUG_SETTLE_ARRIVAL_COUNT`, default off). Before its reply
// each suspension waits for every running thread to reach its suspend point
// (`debug::await_suspension_settled`); with the switch off every round of
// that wait walks all N threads under the debug-state lock the arriving
// threads publish under, with it on a round walks only when an arrival
// moved the per-VM count, and only the threads still on their way.
//
// Stdout is behavioural and must equal HotSpot 25's; the timing row goes to
// stderr (`ns/op` of one suspend + resume of the whole VM), to be compared
// switch on / off (also with CRATONVM_DEBUG_THREAD_SUSPEND_WORD=1) on the
// host and against HotSpot.
//
// Plain run (no debugger): stdout is `ended`.
//
// Under a debugger:
//
//   javac -g -d out L6W64JdiSuspendAllSettleUnderManyThreads.java
//   cratonvm --java-home $JDK --jdwp-port 5803 -cp out L6W64JdiSuspendAllSettleUnderManyThreads wait
//   java -cp out L6W64JdiSuspendAllSettleUnderManyThreads debug 5803 > transcript.txt 2> timings.txt
//   (and the same with CRATONVM_DEBUG_SETTLE_ARRIVAL_COUNT=1 on the debuggee)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// == attach
//   spinners: 256
//   round trips: 100 complete at each reply: true held still each time: true
//   ran after the resume: true
// == end
//   disconnected
// HOTSPOT_EXPECTED_END
//
// The debuggee's own stdout is `ended` in both runs. `CRATONVM_FRAME_TRACE=1`
// prints `[SUSPEND_SETTLED] ... walks=<n> polls=<n> ...` for each settle
// wait that found a thread on its way: with the switch on, `walks` is a
// handful where `polls` counts every round (the positive control); off,
// the two are equal plus one.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;

public class L6W64JdiSuspendAllSettleUnderManyThreads {
    static final int ROUND_TRIPS = 100;
    /** Set by the debugger: 1 = start, 3 = done. */
    static volatile int phase;
    /** Spinners started so far. */
    static volatile int started;
    /** One counter per spinner. */
    static long[] counts;

    static void spin(int slot) {
        synchronized (L6W64JdiSuspendAllSettleUnderManyThreads.class) {
            started++;
        }
        long[] mine = counts;
        while (phase != 3) {
            mine[slot]++;
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length >= 2 && args[0].equals("debug")) {
            int n = args.length >= 3 ? Integer.parseInt(args[2]) : 256;
            Debugger.run(Integer.parseInt(args[1]), n);
            return;
        }
        boolean debugged = args.length >= 1 && args[0].equals("wait");
        int n = args.length >= 2 ? Integer.parseInt(args[1]) : 256;
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
            type = (ClassType) awaitClass("L6W64JdiSuspendAllSettleUnderManyThreads");
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

            // VM-wide round trips: each suspension must be complete when its
            // reply arrives -- every spinner suspended and still.
            boolean completeEachTime = true;
            boolean stillEachTime = true;
            long start = System.nanoTime();
            for (int i = 0; i < ROUND_TRIPS; i++) {
                vm.suspend();
                for (ThreadReference t : spinners) {
                    completeEachTime &= t.isSuspended();
                }
                long s = sum();
                stillEachTime &= sum() == s;
                vm.resume();
            }
            long roundTripNs = (System.nanoTime() - start) / ROUND_TRIPS;
            System.out.println("  round trips: " + ROUND_TRIPS + " complete at each reply: "
                    + completeEachTime + " held still each time: " + stillEachTime);

            long after = sum();
            long until2 = System.currentTimeMillis() + 10_000;
            while (sum() == after && System.currentTimeMillis() < until2) {
                Thread.sleep(1);
            }
            System.out.println("  ran after the resume: " + (sum() != after));
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
