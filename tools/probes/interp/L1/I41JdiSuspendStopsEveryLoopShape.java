// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Shape 4 of
// docs/internal/fixed-bugs/i41-L1-compiled-bodies-without-an-exit-finish-their-activation-compiled-FIXED-20261007.md:
// a JDWP suspension stops a thread wherever it runs. HotSpot's suspension is
// a handshake, so a thread spinning in compiled code stops at once; CratonVM
// sends a compiled loop to the interpreter at its next poll and parks it
// there, so every loop shape needs a poll that can leave. Each spinner runs
// one shape of loop the i41 page tracked, hot (compiled), bumping its own
// counter; the debugger suspends the VM and reads each counter twice, 300 ms
// apart, then resumes and does it again with `ThreadReference.suspend()`.
//
// Rows, per shape: `<shape> vm stable: <bool> thread stable: <bool>`.
//   plain     a counting loop
//   sync      a loop in a `static synchronized` method
//   block     a loop inside `synchronized (LOCK)`
//   iterator  a for-each over a range whose iterator is scalar-replaced
//   nested    a loop whose body splices a method that splices another, which
//             makes a real call
//
// Canonical transcript: no ids, no addresses, no timings.
//
// HotSpot 25.0.3 as the debuggee (default flags, the Windows box):
//
//   == attach
//     plain vm stable: true thread stable: true
//     sync vm stable: true thread stable: true
//     block vm stable: true thread stable: true
//     iterator vm stable: true thread stable: true
//     nested vm stable: true thread stable: true
//   == end
//     disconnected
//
// Plain run (no debugger): `ended`.
//
// Under a debugger:
//   javac -g -d out I41JdiSuspendStopsEveryLoopShape.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out I41JdiSuspendStopsEveryLoopShape wait
//   java -cp out I41JdiSuspendStopsEveryLoopShape debug 5791
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import java.util.*;

public class I41JdiSuspendStopsEveryLoopShape {
    static volatile int phase;
    static volatile boolean stop;
    static final String[] SHAPES = {"plain", "sync", "block", "iterator", "nested"};
    static final long[] COUNTERS = new long[SHAPES.length];
    static volatile long plainCount;
    static volatile long syncCount;
    static volatile long blockCount;
    static volatile long iteratorCount;
    static volatile long nestedCount;
    static final Object LOCK = new Object();

    static void plain() {
        while (!stop) {
            plainCount++;
        }
    }

    static synchronized void sync() {
        while (!stop) {
            syncCount++;
        }
    }

    static void block() {
        synchronized (LOCK) {
            while (!stop) {
                blockCount++;
            }
        }
    }

    static final class Range {
        final long limit;

        Range(long limit) {
            this.limit = limit;
        }

        It iterator() {
            return new It(limit);
        }
    }

    static final class It {
        long cursor;
        final long limit;

        It(long limit) {
            this.limit = limit;
        }

        boolean hasNext() {
            return cursor < limit && !stop;
        }

        long next() {
            return cursor++;
        }
    }

    static void iterator() {
        for (It it = new Range(Long.MAX_VALUE).iterator(); it.hasNext(); ) {
            iteratorCount += it.next() & 1;
        }
    }

    static int leaf(int x) {
        return x + 1;
    }

    static int inner(int x) {
        return leaf(x) + 1;
    }

    static int outer(int x) {
        return inner(x) + 1;
    }

    static void nested() {
        while (!stop) {
            nestedCount += outer((int) nestedCount) & 1;
        }
    }

    static long read(int shape) {
        switch (shape) {
            case 0: return plainCount;
            case 1: return syncCount;
            case 2: return blockCount;
            case 3: return iteratorCount;
            default: return nestedCount;
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean debugged = args.length == 1 && args[0].equals("wait");
        Runnable[] bodies = {
            I41JdiSuspendStopsEveryLoopShape::plain,
            I41JdiSuspendStopsEveryLoopShape::sync,
            I41JdiSuspendStopsEveryLoopShape::block,
            I41JdiSuspendStopsEveryLoopShape::iterator,
            I41JdiSuspendStopsEveryLoopShape::nested,
        };
        List<Thread> threads = new ArrayList<>();
        for (int i = 0; i < SHAPES.length; i++) {
            Thread t = new Thread(bodies[i], "spinner-" + SHAPES[i]);
            t.setDaemon(true);
            t.start();
            threads.add(t);
        }
        long until = System.currentTimeMillis() + 300_000;
        if (debugged) {
            phase = 2;
            while (phase != 3 && System.currentTimeMillis() < until) {
                Thread.sleep(10);
            }
        } else {
            Thread.sleep(500);
        }
        stop = true;
        for (Thread t : threads) {
            t.join(10_000);
        }
        System.out.println("ended");
    }

    static final class Debugger {
        static void run(int port) throws Exception {
            AttachingConnector conn = null;
            for (AttachingConnector c : Bootstrap.virtualMachineManager().attachingConnectors()) {
                if (c.transport().name().equals("dt_socket")) {
                    conn = c;
                }
            }
            Map<String, Connector.Argument> a = conn.defaultArguments();
            a.get("port").setValue(Integer.toString(port));
            a.get("hostname").setValue("localhost");
            VirtualMachine vm = conn.attach(a);
            System.out.println("== attach");
            ReferenceType main = vm.classesByName("I41JdiSuspendStopsEveryLoopShape").get(0);
            Field phase = main.fieldByName("phase");
            long until = System.currentTimeMillis() + 120_000;
            while (((IntegerValue) main.getValue(phase)).value() != 2 && System.currentTimeMillis() < until) {
                Thread.sleep(20);
            }
            // Let every loop run hot: compiled bodies, not the interpreter.
            Thread.sleep(3000);
            Map<String, ThreadReference> byName = new HashMap<>();
            for (ThreadReference t : vm.allThreads()) {
                byName.put(t.name(), t);
            }
            String[] fields = {"plainCount", "syncCount", "blockCount", "iteratorCount", "nestedCount"};
            for (int i = 0; i < SHAPES.length; i++) {
                Field f = main.fieldByName(fields[i]);
                vm.suspend();
                long a1 = ((LongValue) main.getValue(f)).value();
                Thread.sleep(300);
                long a2 = ((LongValue) main.getValue(f)).value();
                vm.resume();
                ThreadReference t = byName.get("spinner-" + SHAPES[i]);
                t.suspend();
                long b1 = ((LongValue) main.getValue(f)).value();
                Thread.sleep(300);
                long b2 = ((LongValue) main.getValue(f)).value();
                t.resume();
                System.out.println("  " + SHAPES[i] + " vm stable: " + (a1 == a2) + " thread stable: " + (b1 == b2));
            }
            ((ClassType) main).setValue(phase, vm.mirrorOf(3));
            System.out.println("== end");
            vm.dispose();
            System.out.println("  disconnected");
        }
    }
}
