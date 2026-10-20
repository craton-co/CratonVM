// Interpreter round i2 wave 65, lane L6: N spinning threads (default 64;
// `debug <port> <n>` and `wait <n>` change it) held by
// `ThreadReference.suspend()` while an unsuspended thread allocates and runs
// `System.gc()` in a loop -- the scenario of
// docs/internal/fixed-bugs/interpreter-i64-L6-proposal-a-word-parked-debuggee-parks-gc-blocked-KEPT-OFF-20261104.md
// (`CRATONVM_DEBUG_PARK_GC_BLOCKED`, default off, which needs
// `CRATONVM_DEBUG_THREAD_SUSPEND_WORD`, default on). With the switch each
// parked spinner waits inside a GC-blocked region: the collections neither
// wake it nor wait for it, and remap its frames through its deposit.
//
// What it checks (stdout, behavioural, HotSpot 25's):
// - while held and collected around, each spinner's JDWP status is RUNNING
//   and its `Thread.getState()`, read by the collecting thread, RUNNABLE (a
//   GC-blocked park must not read as WAITING / TIMED_WAITING / BLOCKED);
// - after the collections, an object each spinner's frame holds (`box`,
//   allocated before the hold and moved by the collections here) still reads
//   its own field through the frame (`StackFrame.getValue` then
//   `ObjectReference.getValue`, a heap read run on a parked thread);
// - a frame write (`StackFrame.setValue` of the local `k`), which runs on the
//   spinner itself and must leave its region first, is accepted and seen by
//   the spinner after the resume.
//
// Stderr (timings, not compared): `System.gc() (N suspended): <ns>/op` and
// `ObjectReference.getValue (N suspended): <ns>/op` -- the first is the
// time-to-safepoint the stage removes, the second what each parked heap read
// now pays (the parked thread leaves and re-enters its region). Compare
// `CRATONVM_DEBUG_PARK_GC_BLOCKED=1` against off, and against HotSpot; also
// run `L6W64JdiGcWhileThreadsSuspended` both ways.
//
// Plain run (no debugger): stdout is `ended`.
//
// Under a debugger:
//
//   javac -g -d out L6W65JdiGcBlockedParks.java
//   cratonvm --java-home $JDK --jdwp-port 5803 -cp out L6W65JdiGcBlockedParks wait
//   java -cp out L6W65JdiGcBlockedParks debug 5803 > transcript.txt 2> timings.txt
//   (and again with CRATONVM_DEBUG_PARK_GC_BLOCKED=1 on the debuggee)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// == attach
//   spinners: 64
//   suspended: 64
//   collections while held: true
//   held still while collecting: true
//   getState RUNNABLE while held: true
//   statuses RUNNING while held: true
//   boxes intact after collections: true
//   frame writes accepted: true
//   still suspended: 64
//   ran after the resume: true
//   frame writes seen after the resume: true
// == end
//   disconnected
// HOTSPOT_EXPECTED_END
//
// The debuggee's own stdout is `ended` in both runs. With
// `CRATONVM_FRAME_TRACE=1` each park prints
// `[DEBUG_PARK_WORD] tid=<n> rounds=<n> locked=<n> word_wakes=<n> stw_unparks=<bool> gc_blocked=<bool> regions=<n>`:
// with `gc_blocked=true`, `regions` is 1 plus one per task the spinner ran
// (its frame write, the heap reads routed to it), and `rounds` does not grow
// with the number of collections.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;

public class L6W65JdiGcBlockedParks {
    static final long HOLD_MS = 2_000;
    /** Set by the debugger: 1 = start, 2 = collect, 4 = stop collecting, 3 = done. */
    static volatile int phase;
    /** Spinners started so far. */
    static volatile int started;
    /** Collections the collector thread ran in phase 2, and their total time. */
    static volatile int gcCount;
    static volatile long gcNanos;
    /** `Thread.getState()` reads of the spinners in phase 2, and how many were not RUNNABLE. */
    static volatile int stateReads;
    static volatile int notRunnable;
    /** The collector saw phase 2 end. */
    static volatile int collectorDone;
    /** One counter per spinner. */
    static long[] counts;
    /** What each spinner saw of its frame write (`k`), 0 until it did. */
    static int[] seen;
    static Thread[] spinnerThreads;
    /** Kept so the allocations between collections are not dead code. */
    static volatile int junkLength;

    static final class Box {
        final int slot;

        Box(int slot) {
            this.slot = slot;
        }
    }

    static void spin(int slot) {
        Box box = new Box(slot);
        int k = 0;
        synchronized (L6W65JdiGcBlockedParks.class) {
            started++;
        }
        long[] mine = counts;
        while (phase != 3) {
            mine[slot]++;
            if (k != 0) {
                seen[box.slot] = k;
                k = 0;
            }
        }
    }

    static void collect() {
        while (phase != 3 && phase != 2) {
            Thread.onSpinWait();
        }
        while (phase == 2) {
            byte[][] junk = new byte[256][];
            for (int i = 0; i < junk.length; i++) {
                junk[i] = new byte[512];
            }
            junkLength = junk.length;
            long t0 = System.nanoTime();
            System.gc();
            gcNanos += System.nanoTime() - t0;
            gcCount++;
            for (Thread t : spinnerThreads) {
                if (t.getState() != Thread.State.RUNNABLE) {
                    notRunnable++;
                }
                stateReads++;
            }
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
        seen = new int[n];
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
        }
        spinnerThreads = spinners;
        Thread collector = new Thread(L6W65JdiGcBlockedParks::collect, "collector");
        collector.setDaemon(true);
        collector.start();
        for (Thread t : spinners) {
            t.start();
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

        static int slotOf(ThreadReference t) {
            return Integer.parseInt(t.name().substring("spinner-".length()));
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
            type = (ClassType) awaitClass("L6W65JdiGcBlockedParks");
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
            System.out.println("  getState RUNNABLE while held: "
                    + (intField("stateReads") > 0 && intField("notRunnable") == 0));
            int running = 0;
            for (ThreadReference t : spinners) {
                if (t.status() == ThreadReference.THREAD_STATUS_RUNNING) {
                    running++;
                }
            }
            System.out.println("  statuses RUNNING while held: " + (running == spinners.size()));

            int intact = 0;
            ObjectReference someBox = null;
            Field slotField = null;
            for (ThreadReference t : spinners) {
                StackFrame f = t.frame(0);
                LocalVariable boxVar = f.visibleVariableByName("box");
                ObjectReference box = boxVar == null ? null : (ObjectReference) f.getValue(boxVar);
                if (box == null) {
                    continue;
                }
                if (slotField == null) {
                    slotField = box.referenceType().fieldByName("slot");
                }
                if (((IntegerValue) box.getValue(slotField)).value() == slotOf(t)) {
                    intact++;
                }
                if (someBox == null) {
                    someBox = box;
                }
            }
            System.out.println("  boxes intact after collections: " + (intact == spinners.size()));

            if (someBox != null) {
                final int reads = 500;
                long t0 = System.nanoTime();
                for (int i = 0; i < reads; i++) {
                    someBox.getValue(slotField);
                }
                long readNs = (System.nanoTime() - t0) / reads;
                System.err.println("ObjectReference.getValue (" + spinners.size() + " suspended): "
                        + readNs + " ns/op");
            }

            int written = 0;
            for (ThreadReference t : spinners) {
                StackFrame f = t.frame(0);
                LocalVariable kVar = f.visibleVariableByName("k");
                if (kVar == null) {
                    continue;
                }
                try {
                    f.setValue(kVar, vm.mirrorOf(1000 + slotOf(t)));
                    written++;
                } catch (InvalidTypeException | ClassNotLoadedException e) {
                    System.out.println("  setValue failed: " + e);
                }
            }
            System.out.println("  frame writes accepted: " + (written == spinners.size()));

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
            boolean allSeen = false;
            until = System.currentTimeMillis() + 10_000;
            while (!allSeen && System.currentTimeMillis() < until) {
                ArrayReference seenArray = (ArrayReference) type.getValue(type.fieldByName("seen"));
                allSeen = true;
                List<Value> values = seenArray.getValues();
                for (int i = 0; i < values.size(); i++) {
                    if (((IntegerValue) values.get(i)).value() != 1000 + i) {
                        allSeen = false;
                    }
                }
                if (!allSeen) {
                    Thread.sleep(5);
                }
            }
            System.out.println("  frame writes seen after the resume: " + allSeen);
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
