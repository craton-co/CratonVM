// Interpreter round i2 wave 66, lane L6: N spinning threads (default 64),
// each spinning D frames down (default 200), held by
// `ThreadReference.suspend()` while the debugger reads the heap -- the
// scenario of
// docs/internal/fixed-bugs/interpreter-i65-L6-proposal-heap-work-prefers-a-thread-outside-its-region-KEPT-OFF-20261104.md
// (`CRATONVM_DEBUG_HEAP_WORK_OUTSIDE_REGION`) and
// docs/internal/fixed-bugs/interpreter-i65-L6-proposal-a-parked-region-reuses-its-deposit-KEPT-OFF-20261104.md
// (`CRATONVM_DEBUG_PARK_REGION_REUSES_DEPOSIT`). Both switches are default
// off and act only with `CRATONVM_DEBUG_PARK_GC_BLOCKED=1`, under which a
// parked spinner waits inside a GC-blocked region and, in wave 65, left and
// re-entered it (a deposit of its whole stack) for every heap read the
// JDWP server handed it.
//
// What it checks (stdout, behavioural, HotSpot 25's):
// - while held, each spinner's JDWP status is RUNNING and the counters do
//   not move across the reads;
// - the object each spinner's top frame holds (`box`) reads its own field
//   (`ObjectReference.getValue`, heap work), many times over;
// - a string mirrored into the debuggee (`VirtualMachine.mirrorOf`, heap
//   work that allocates) reads back;
// - a frame write (`StackFrame.setValue` of `k`, work on the spinner itself)
//   is accepted, and heap reads after it still answer; the spinner sees the
//   write after its resume;
// - the spinners run again after rounds of `VirtualMachine.suspend()` /
//   `resume()`.
//
// Stderr (timings, not compared):
//   `ObjectReference.getValue (N suspended, depth D): <ns>/op`
//   `ObjectReference.getValue after a frame write (N suspended, depth D): <ns>/op`
//   `VirtualMachine.mirrorOf(String) (N suspended): <ns>/op`
//   `VirtualMachine.suspend+resume (N spinners, depth D): <ns>/op`
// Compare `CRATONVM_DEBUG_PARK_GC_BLOCKED=1` alone, with
// `CRATONVM_DEBUG_HEAP_WORK_OUTSIDE_REGION=1`, with
// `CRATONVM_DEBUG_PARK_REGION_REUSES_DEPOSIT=1`, with both, and with
// `CRATONVM_DEBUG_PARK_GC_BLOCKED` unset; and against HotSpot.
//
// Plain run (no debugger): stdout is `ended`.
//
// Under a debugger:
//
//   javac -g -d out L6W66JdiParkedHeapWork.java
//   cratonvm --java-home $JDK --jdwp-port 5806 -cp out L6W66JdiParkedHeapWork wait [n] [depth]
//   java -cp out L6W66JdiParkedHeapWork debug 5806 [n] [depth] > transcript.txt 2> timings.txt
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// == attach
//   spinners: 64
//   suspended: 64
//   statuses RUNNING while held: true
//   boxes intact: true
//   repeated heap reads answer: true
//   string mirrored: true
//   held still while reading: true
//   frame writes accepted: true
//   heap reads after the frame writes answer: true
//   still suspended: 64
//   ran after the resume: true
//   frame writes seen after the resume: true
//   ran after suspend+resume rounds: true
// == end
//   disconnected
// HOTSPOT_EXPECTED_END
//
// The debuggee's own stdout is `ended` in both runs. With
// `CRATONVM_FRAME_TRACE=1` each park prints
// `[DEBUG_PARK_WORD] tid=<n> ... gc_blocked=<bool> regions=<n> reuses=<n> region_enter_us=<n> region_leave_us=<n>`.
// With `CRATONVM_DEBUG_HEAP_WORK_OUTSIDE_REGION=1` the lowest-numbered
// spinner's `regions` stays at 1 plus its frame write's re-entry (the reads
// go to the heap service); with `CRATONVM_DEBUG_PARK_REGION_REUSES_DEPOSIT=1`
// alone its `reuses` is about the number of reads it ran, and
// `region_enter_us` falls with it.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;

public class L6W66JdiParkedHeapWork {
    /** Set by the debugger: 1 = start, 3 = done. */
    static volatile int phase;
    /** Spinners at the bottom of their dive so far. */
    static volatile int started;
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
        synchronized (L6W66JdiParkedHeapWork.class) {
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
        Thread[] spinners = new Thread[n];
        for (int i = 0; i < n; i++) {
            int slot = i;
            spinners[i] = new Thread(() -> dive(slot, depth), "spinner-" + i);
            spinners[i].setDaemon(true);
        }
        for (Thread t : spinners) {
            t.start();
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
        static final int READS = 500;
        static final int ROUNDS = 20;
        static VirtualMachine vm;
        static ClassType type;

        static int intField(String name) {
            return ((IntegerValue) type.getValue(type.fieldByName(name))).value();
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
            type = (ClassType) awaitClass("L6W66JdiParkedHeapWork");
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
            // Let every park settle into its region before the reads.
            Thread.sleep(200);

            int running = 0;
            for (ThreadReference t : spinners) {
                if (t.status() == ThreadReference.THREAD_STATUS_RUNNING) {
                    running++;
                }
            }
            System.out.println("  statuses RUNNING while held: " + (running == spinners.size()));

            long before = sum();
            int intact = 0;
            ObjectReference someBox = null;
            int someSlot = -1;
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
                    someSlot = slotOf(t);
                }
            }
            System.out.println("  boxes intact: " + (intact == spinners.size()));

            long readNs = someBox == null ? -1 : timeReads(someBox, slotField, someSlot);
            System.out.println("  repeated heap reads answer: " + (readNs >= 0));
            if (readNs >= 0) {
                System.err.println("ObjectReference.getValue (" + spinners.size() + " suspended, depth "
                        + depth + "): " + readNs + " ns/op");
            }

            boolean mirrored = true;
            long t0 = System.nanoTime();
            for (int i = 0; i < READS / 10; i++) {
                StringReference s = vm.mirrorOf("w66-" + i);
                if (!s.value().equals("w66-" + i)) {
                    mirrored = false;
                }
            }
            long mirrorNs = (System.nanoTime() - t0) / (READS / 10);
            System.out.println("  string mirrored: " + mirrored);
            System.err.println("VirtualMachine.mirrorOf(String) (" + spinners.size() + " suspended): "
                    + mirrorNs + " ns/op");
            System.out.println("  held still while reading: " + (sum() == before));

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
            long afterWriteNs = someBox == null ? -1 : timeReads(someBox, slotField, someSlot);
            System.out.println("  heap reads after the frame writes answer: " + (afterWriteNs >= 0));
            if (afterWriteNs >= 0) {
                System.err.println("ObjectReference.getValue after a frame write (" + spinners.size()
                        + " suspended, depth " + depth + "): " + afterWriteNs + " ns/op");
            }

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

            long roundsNs = 0;
            for (int r = 0; r < ROUNDS; r++) {
                long s0 = System.nanoTime();
                vm.suspend();
                vm.resume();
                roundsNs += System.nanoTime() - s0;
                Thread.sleep(5);
            }
            System.err.println("VirtualMachine.suspend+resume (" + spinners.size() + " spinners, depth "
                    + depth + "): " + (roundsNs / ROUNDS) + " ns/op");
            long afterRounds = sum();
            until = System.currentTimeMillis() + 10_000;
            while (sum() == afterRounds && System.currentTimeMillis() < until) {
                Thread.sleep(1);
            }
            System.out.println("  ran after suspend+resume rounds: " + (sum() != afterRounds));

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
