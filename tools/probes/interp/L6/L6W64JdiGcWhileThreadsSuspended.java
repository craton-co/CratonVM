// Interpreter round i2 wave 64, lane L6: N spinning threads (default 64;
// `debug <port> <n>` and `wait <n>` change it) held by
// `ThreadReference.suspend()` while an unsuspended thread runs `System.gc()`
// in a loop -- the measurement scenario of
// docs/internal/fixed-bugs/interpreter-i63-L6-proposal-a-safepoint-request-unparks-word-parked-debuggees-KEPT-OFF-20261104.md
// (`CRATONVM_DEBUG_STW_UNPARKS_PARKED`, default off, which needs
// `CRATONVM_DEBUG_THREAD_SUSPEND_WORD`). Each parked thread is a mutator the
// collection waits for: with the word alone it wakes every 1 ms to look at
// the stop-the-world flag (and a collection waits up to that 1 ms per
// straggler); with both switches it waits on its word up to the word's 50 ms
// backstop and the collection's request unparks it.
//
// Stdout is behavioural and must equal HotSpot 25's; the timing rows go to
// stderr (`ns/op` of one `System.gc()` while the N threads are held), to be
// compared across the three settings (word off; word on; word + unpark) on
// the host and against HotSpot.
//
// Plain run (no debugger): stdout is `ended`.
//
// Under a debugger:
//
//   javac -g -d out L6W64JdiGcWhileThreadsSuspended.java
//   cratonvm --java-home $JDK --jdwp-port 5802 -cp out L6W64JdiGcWhileThreadsSuspended wait
//   java -cp out L6W64JdiGcWhileThreadsSuspended debug 5802 > transcript.txt 2> timings.txt
//   (and again with CRATONVM_DEBUG_THREAD_SUSPEND_WORD=1, then with that and
//   CRATONVM_DEBUG_STW_UNPARKS_PARKED=1, on the debuggee)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// == attach
//   spinners: 64
//   suspended: 64
//   collections while held: true
//   held still while collecting: true
//   still suspended: 64
//   ran after the resume: true
// == end
//   disconnected
// HOTSPOT_EXPECTED_END
//
// The debuggee's own stdout is `ended` in both runs. With
// `CRATONVM_FRAME_TRACE=1` each park prints
// `[DEBUG_PARK_WORD] tid=<n> rounds=<n> locked=<n> word_wakes=<n> stw_unparks=<bool>`:
// with `stw_unparks=true`, `rounds` over the ~2 s hold is a few dozen plus
// two per collection (the positive control), where the 1 ms poll gives ~2 000.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;

public class L6W64JdiGcWhileThreadsSuspended {
    static final long HOLD_MS = 2_000;
    /** Set by the debugger: 1 = start, 2 = collect, 4 = stop collecting, 3 = done. */
    static volatile int phase;
    /** Spinners started so far. */
    static volatile int started;
    /** Collections the collector thread ran in phase 2, and their total time. */
    static volatile int gcCount;
    static volatile long gcNanos;
    /** The collector saw phase 2 end. */
    static volatile int collectorDone;
    /** One counter per spinner. */
    static long[] counts;

    static void spin(int slot) {
        synchronized (L6W64JdiGcWhileThreadsSuspended.class) {
            started++;
        }
        long[] mine = counts;
        while (phase != 3) {
            mine[slot]++;
        }
    }

    static void collect() {
        while (phase != 3 && phase != 2) {
            Thread.onSpinWait();
        }
        while (phase == 2) {
            long t0 = System.nanoTime();
            System.gc();
            gcNanos += System.nanoTime() - t0;
            gcCount++;
        }
        collectorDone = 1;
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
        Thread collector = new Thread(L6W64JdiGcWhileThreadsSuspended::collect, "collector");
        collector.setDaemon(true);
        collector.start();
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
        collector.join(10_000);
        System.out.println("ended");
    }

    static final class Debugger {
        static VirtualMachine vm;
        static ClassType type;

        static int intField(String name) {
            return ((IntegerValue) type.getValue(type.fieldByName(name))).value();
        }

        static long longField(String name) {
            return ((LongValue) type.getValue(type.fieldByName(name))).value();
        }

        static void setPhase(int p) throws Exception {
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(p));
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
            type = (ClassType) awaitClass("L6W64JdiGcWhileThreadsSuspended");
            setPhase(1);
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

            for (ThreadReference t : spinners) {
                t.suspend();
            }
            int suspended = 0;
            for (ThreadReference t : spinners) {
                if (t.isSuspended()) {
                    suspended++;
                }
            }
            System.out.println("  suspended: " + suspended);

            long before = sum();
            setPhase(2);
            Thread.sleep(HOLD_MS);
            setPhase(4);
            until = System.currentTimeMillis() + 60_000;
            while (intField("collectorDone") == 0) {
                if (System.currentTimeMillis() > until) {
                    fail("the collector did not stop");
                }
                Thread.sleep(10);
            }
            int gcs = intField("gcCount");
            long gcNs = longField("gcNanos");
            System.out.println("  collections while held: " + (gcs > 0));
            System.out.println("  held still while collecting: " + (sum() == before));
            int still = 0;
            for (ThreadReference t : spinners) {
                if (t.isSuspended()) {
                    still++;
                }
            }
            System.out.println("  still suspended: " + still);

            for (ThreadReference t : spinners) {
                t.resume();
            }
            long after = sum();
            until = System.currentTimeMillis() + 10_000;
            while (sum() == after && System.currentTimeMillis() < until) {
                Thread.sleep(1);
            }
            System.out.println("  ran after the resume: " + (sum() != after));
            if (gcs > 0) {
                System.err.println("System.gc() (" + spinners.size() + " suspended): "
                        + (gcNs / gcs) + " ns/op (" + gcs + " collections)");
            }

            setPhase(3);
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
