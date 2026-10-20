// Interpreter round i2 wave 68, lane L6: N spinning threads (default 64),
// each spinning D frames down (default 200), held by the debugger through
// holds that see no collection and holds that see several -- the scenario of
// docs/internal/fixed-bugs/interpreter-i66-L6-proposal-a-park-opens-its-region-at-the-first-pause-KEPT-OFF-20261104.md
// (`CRATONVM_DEBUG_PARK_REGION_AT_FIRST_PAUSE`) and
// docs/internal/fixed-bugs/interpreter-i66-L6-proposal-a-park-leave-keeps-its-snapshot-when-nothing-folded-KEPT-OFF-20261104.md
// (`CRATONVM_DEBUG_PARK_LEAVE_KEEPS_UNFOLDED_SNAPSHOT`). Both switches are
// default off. The first acts only with `CRATONVM_DEBUG_PARK_GC_BLOCKED=1`:
// a parked spinner then waits outside its GC-blocked region until the first
// pause of its hold, and opens the region at that pause. The second acts
// only with `CRATONVM_DEBUG_PARK_REGION_REUSES_DEPOSIT=1` as well: a region
// left for a heap read keeps its entry's root snapshot when nothing folded
// into it.
//
// What it checks (stdout, behavioural, HotSpot 25's):
// - the spinners run again after rounds of `VirtualMachine.suspend()` /
//   `resume()` that see no collection;
// - while held, each spinner's JDWP status is RUNNING, its counter does not
//   move, and the object its top frame holds (`box`) reads its own field,
//   many times over, before any collection;
// - collections run on another thread while the spinners are held (a
//   `gc-driver` thread calls `System.gc()` on the debugger's request), the
//   counters still do not move, and every `box` still reads its own field
//   after them (read through the frames listed again);
// - heap reads after the collections still answer;
// - a frame write (`StackFrame.setValue` of `k`) is accepted and the spinner
//   sees it after its resume.
//
// Stderr (timings, not compared):
//   `VirtualMachine.suspend+resume, no collection (N spinners, depth D): <ns>/op`
//   `ObjectReference.getValue, no collection in the hold (N suspended, depth D): <ns>/op`
//   `System.gc() while held (N suspended, depth D): <ns>/op`
//   `ObjectReference.getValue after collections (N suspended, depth D): <ns>/op`
// Measure: the four combinations of `CRATONVM_DEBUG_PARK_GC_BLOCKED=1` with
// `CRATONVM_DEBUG_PARK_REGION_AT_FIRST_PAUSE` x
// `CRATONVM_DEBUG_PARK_REGION_REUSES_DEPOSIT` (+
// `CRATONVM_DEBUG_PARK_LEAVE_KEEPS_UNFOLDED_SNAPSHOT` with it), plus
// `CRATONVM_DEBUG_PARK_GC_BLOCKED` unset as the wave-64 baseline,
// interleaved run by run, at least five runs each, medians; and HotSpot.
//
// Plain run (no debugger): stdout is `ended`.
//
// Under a debugger:
//
//   javac -g -d out L6W68JdiLazyParkRegion.java
//   cratonvm --java-home $JDK --jdwp-port 5808 -cp out L6W68JdiLazyParkRegion wait [n] [depth]
//   java -cp out L6W68JdiLazyParkRegion debug 5808 [n] [depth] > transcript.txt 2> timings.txt
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// == attach
//   spinners: 64
//   ran after suspend+resume rounds: true
//   suspended: 64
//   statuses RUNNING while held: true
//   boxes intact: true
//   repeated heap reads answer: true
//   collections ran while held: true
//   held still through the collections: true
//   boxes intact after the collections: true
//   heap reads after the collections answer: true
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
// `[DEBUG_PARK_WORD] tid=<n> ... regions=<n> reuses=<n> region_enter_us=<n> region_leave_us=<n> lazy=<bool> pause_entries=<n> keeps=<n>`.
// With `CRATONVM_DEBUG_PARK_REGION_AT_FIRST_PAUSE=1`, every park of the
// suspend+resume rounds shows `regions=0`, and each spinner's long hold
// shows `pause_entries=1` (the first `System.gc()` opened its region; the
// later ones neither woke it nor waited for it). With
// `CRATONVM_DEBUG_PARK_LEAVE_KEEPS_UNFOLDED_SNAPSHOT=1`, the spinner that ran
// the reads after the collections shows `keeps` about equal to its `reuses`.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;

public class L6W68JdiLazyParkRegion {
    /** Set by the debugger: 1 = start, 3 = done. */
    static volatile int phase;
    /** Spinners at the bottom of their dive so far. */
    static volatile int started;
    /** Collections the debugger asked the gc-driver for. */
    static volatile int gcRequests;
    /** Collections the gc-driver ran. */
    static volatile int gcDone;
    /** Nanoseconds the gc-driver spent in `System.gc()`. */
    static volatile long gcNanos;
    /** One counter per spinner. */
    static long[] counts;
    /** What each spinner saw of its frame write (`k`), 0 until it did. */
    static int[] seen;

    static final class Box {
        final int slot;

        Box(int slot) {
            this.slot = slot;
        }
    }

    static void dive(int slot, int depth) {
        if (depth > 0) {
            dive(slot, depth - 1);
            return;
        }
        spin(slot);
    }

    static void spin(int slot) {
        Box box = new Box(slot);
        int k = 0;
        synchronized (L6W68JdiLazyParkRegion.class) {
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

    /** Runs `System.gc()` once per request the debugger makes. */
    static void driveCollections() {
        while (phase != 3) {
            if (gcDone < gcRequests) {
                // Garbage for the collection to find.
                Object[] junk = new Object[1024];
                for (int i = 0; i < junk.length; i++) {
                    junk[i] = new int[16];
                }
                long t0 = System.nanoTime();
                System.gc();
                gcNanos += System.nanoTime() - t0;
                gcDone++;
            } else {
                try {
                    Thread.sleep(1);
                } catch (InterruptedException e) {
                    return;
                }
            }
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length >= 2 && args[0].equals("debug")) {
            int n = args.length >= 3 ? Integer.parseInt(args[2]) : 64;
            int depth = args.length >= 4 ? Integer.parseInt(args[3]) : 200;
            Debugger.run(Integer.parseInt(args[1]), n, depth);
            return;
        }
        boolean debugged = args.length >= 1 && args[0].equals("wait");
        int n = args.length >= 2 ? Integer.parseInt(args[1]) : 64;
        int depth = args.length >= 3 ? Integer.parseInt(args[2]) : 200;
        counts = new long[n];
        seen = new int[n];
        long until = System.currentTimeMillis() + 300_000;
        while (debugged && phase == 0 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        if (!debugged) {
            phase = 3;
        }
        Thread driver = new Thread(L6W68JdiLazyParkRegion::driveCollections, "gc-driver");
        driver.setDaemon(true);
        Thread[] spinners = new Thread[n];
        for (int i = 0; i < n; i++) {
            int slot = i;
            spinners[i] = new Thread(() -> dive(slot, depth), "spinner-" + i);
            spinners[i].setDaemon(true);
        }
        driver.start();
        for (Thread t : spinners) {
            t.start();
        }
        while (debugged && phase != 3 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        for (Thread t : spinners) {
            t.join(10_000);
        }
        driver.join(10_000);
        System.out.println("ended");
    }

    static final class Debugger {
        static final int READS = 500;
        static final int ROUNDS = 20;
        static final int COLLECTIONS = 10;
        static VirtualMachine vm;
        static ClassType type;

        static int intField(String name) {
            return ((IntegerValue) type.getValue(type.fieldByName(name))).value();
        }

        static long longField(String name) {
            return ((LongValue) type.getValue(type.fieldByName(name))).value();
        }

        static void setInt(String name, int v) throws Exception {
            type.setValue(type.fieldByName(name), vm.mirrorOf(v));
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

        /** `READS` reads of `box.slot`; the ns/op, or -1 if one answered wrong. */
        static long timeReads(ObjectReference box, Field slotField, int expect) {
            long t0 = System.nanoTime();
            boolean right = true;
            for (int i = 0; i < READS; i++) {
                if (((IntegerValue) box.getValue(slotField)).value() != expect) {
                    right = false;
                }
            }
            long ns = (System.nanoTime() - t0) / READS;
            return right ? ns : -1;
        }

        /** The `box` each spinner's top frame holds, read through its listing now. */
        static Map<ThreadReference, ObjectReference> boxes(List<ThreadReference> spinners) throws Exception {
            Map<ThreadReference, ObjectReference> out = new LinkedHashMap<>();
            for (ThreadReference t : spinners) {
                StackFrame f = t.frame(0);
                LocalVariable boxVar = f.visibleVariableByName("box");
                ObjectReference box = boxVar == null ? null : (ObjectReference) f.getValue(boxVar);
                if (box != null) {
                    out.put(t, box);
                }
            }
            return out;
        }

        /** How many of `boxes` read their own spinner's slot. */
        static int intact(Map<ThreadReference, ObjectReference> boxes, Field slotField) {
            int right = 0;
            for (Map.Entry<ThreadReference, ObjectReference> e : boxes.entrySet()) {
                if (((IntegerValue) e.getValue().getValue(slotField)).value() == slotOf(e.getKey())) {
                    right++;
                }
            }
            return right;
        }

        static void run(int port, int n, int depth) throws Exception {
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
            type = (ClassType) awaitClass("L6W68JdiLazyParkRegion");
            setInt("phase", 1);
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

            // Holds that see no collection: what the lazy region saves.
            long roundsNs = 0;
            for (int r = 0; r < ROUNDS; r++) {
                long s0 = System.nanoTime();
                vm.suspend();
                vm.resume();
                roundsNs += System.nanoTime() - s0;
                Thread.sleep(5);
            }
            System.err.println("VirtualMachine.suspend+resume, no collection (" + spinners.size()
                    + " spinners, depth " + depth + "): " + (roundsNs / ROUNDS) + " ns/op");
            long afterRounds = sum();
            until = System.currentTimeMillis() + 10_000;
            while (sum() == afterRounds && System.currentTimeMillis() < until) {
                Thread.sleep(1);
            }
            System.out.println("  ran after suspend+resume rounds: " + (sum() != afterRounds));

            // One long hold of the spinners only (the gc-driver keeps running).
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
            Thread.sleep(200);

            int running = 0;
            for (ThreadReference t : spinners) {
                if (t.status() == ThreadReference.THREAD_STATUS_RUNNING) {
                    running++;
                }
            }
            System.out.println("  statuses RUNNING while held: " + (running == spinners.size()));

            long before = sum();
            Map<ThreadReference, ObjectReference> held = boxes(spinners);
            Field slotField = null;
            ObjectReference someBox = null;
            int someSlot = -1;
            for (Map.Entry<ThreadReference, ObjectReference> e : held.entrySet()) {
                slotField = e.getValue().referenceType().fieldByName("slot");
                someBox = e.getValue();
                someSlot = slotOf(e.getKey());
                break;
            }
            System.out.println("  boxes intact: "
                    + (slotField != null && intact(held, slotField) == spinners.size()));

            long readNs = someBox == null ? -1 : timeReads(someBox, slotField, someSlot);
            System.out.println("  repeated heap reads answer: " + (readNs >= 0));
            if (readNs >= 0) {
                System.err.println("ObjectReference.getValue, no collection in the hold ("
                        + spinners.size() + " suspended, depth " + depth + "): " + readNs + " ns/op");
            }

            // Collections while the spinners are held: the first is the pause
            // that opens a lazy park's region.
            int asked = intField("gcDone") + COLLECTIONS;
            setInt("gcRequests", asked);
            until = System.currentTimeMillis() + 120_000;
            while (intField("gcDone") < asked && System.currentTimeMillis() < until) {
                Thread.sleep(5);
            }
            boolean collected = intField("gcDone") >= asked;
            System.out.println("  collections ran while held: " + collected);
            if (collected) {
                System.err.println("System.gc() while held (" + spinners.size() + " suspended, depth "
                        + depth + "): " + (longField("gcNanos") / COLLECTIONS) + " ns/op");
            }
            System.out.println("  held still through the collections: " + (sum() == before));
            Map<ThreadReference, ObjectReference> relisted = boxes(spinners);
            System.out.println("  boxes intact after the collections: "
                    + (slotField != null && intact(relisted, slotField) == spinners.size()));

            long afterGcNs = someBox == null ? -1 : timeReads(someBox, slotField, someSlot);
            System.out.println("  heap reads after the collections answer: " + (afterGcNs >= 0));
            if (afterGcNs >= 0) {
                System.err.println("ObjectReference.getValue after collections (" + spinners.size()
                        + " suspended, depth " + depth + "): " + afterGcNs + " ns/op");
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

            setInt("phase", 3);
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
