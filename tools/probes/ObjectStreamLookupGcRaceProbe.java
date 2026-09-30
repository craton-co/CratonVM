// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Targeted stressor for docs/known-issues/jdk-only/junit-launcher-init-classcastexception-
// arraycpliterator-rare-flake-20260919.md's own suggestion: "a loop that does only
// ObjectStreamClass.lookup(X.class) for a Serializable X with a serialVersionUID, from
// several threads while a GC is forced, is the cheapest targeted stressor". The one observed
// occurrence was a ClassCastException (Spliterators$ArraySpliterator -> Object[]) inside
// org.junit.platform.launcher.TestIdentifier's own <clinit>, reached via
// ObjectStreamClass.lookup -- reflection-heavy real-JDK bytecode
// (Class.getDeclaredMethods, AccessController, Modifier) that --jdk-only replaces with
// natives in the default mode, and whose printed stack trace was internally inconsistent
// (frames from calls that cannot have produced each other) -- evidence of frame/stack
// corruption rather than a plain logic bug.
//
//   javac -d out ObjectStreamLookupGcRaceProbe.java
//   cratonvm --jdk-only -cp out ObjectStreamLookupGcRaceProbe [threads] [iters]
//
// Prints only a final summary; any uncaught exception on a worker thread is the reproduction
// and is printed with its full (possibly scrambled) stack trace before the process exits
// non-zero.
import java.io.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;

public class ObjectStreamLookupGcRaceProbe {
    // Mirrors the shape that actually reproduced: a class with instance state, an
    // interface hierarchy and a declared serialVersionUID, not a bare marker class.
    static class Widget implements Serializable, Comparable<Widget> {
        private static final long serialVersionUID = 42L;
        int a; long b; String c; Widget[] kids;
        Widget(int a) { this.a = a; this.b = a; this.c = "w" + a; this.kids = new Widget[0]; }
        public int compareTo(Widget o) { return Integer.compare(a, o.a); }
    }

    static class Gadget implements Serializable {
        private static final long serialVersionUID = 7L;
        double x; Object[] refs = new Object[4];
    }

    public static void main(String[] args) throws Exception {
        int threads = args.length > 0 ? Integer.parseInt(args[0]) : 8;
        int iters = args.length > 1 ? Integer.parseInt(args[1]) : 200_000;
        ExecutorService es = Executors.newFixedThreadPool(threads);
        AtomicLong ok = new AtomicLong();
        AtomicReference<Throwable> firstFailure = new AtomicReference<>();
        CyclicBarrier bar = new CyclicBarrier(threads + 1);

        Runnable gcHammer = () -> {
            try {
                bar.await();
                for (int i = 0; i < iters / 50 && firstFailure.get() == null; i++) {
                    System.gc();
                    // Keep allocating short-lived garbage between forced collections so a
                    // moving young collector actually has something to move.
                    Object[] junk = new Object[256];
                    for (int j = 0; j < junk.length; j++) junk[j] = new Widget(j);
                    if (junk[0] == null) throw new AssertionError();
                }
            } catch (Throwable t) {
                firstFailure.compareAndSet(null, t);
            }
        };
        Thread gcThread = new Thread(gcHammer, "gc-hammer");
        gcThread.setDaemon(true);
        gcThread.start();

        for (int t = 0; t < threads; t++) {
            final int id = t;
            es.submit(() -> {
                try {
                    bar.await();
                    for (int i = 0; i < iters && firstFailure.get() == null; i++) {
                        ObjectStreamClass osc1 = ObjectStreamClass.lookup(Widget.class);
                        ObjectStreamClass osc2 = ObjectStreamClass.lookup(Gadget.class);
                        if (osc1 == null || osc2 == null) {
                            throw new AssertionError("lookup returned null");
                        }
                        // Touch a couple of derived properties, exercising more of the
                        // reflection-heavy path ObjectStreamClass.lookup builds internally
                        // (getFields/getDeclaredMethods/Class.getInterfaces walks).
                        if ((i & 0xFFF) == 0) {
                            osc1.getName();
                            osc2.getFields();
                        }
                        ok.incrementAndGet();
                    }
                } catch (Throwable t2) {
                    firstFailure.compareAndSet(null, t2);
                }
            });
        }
        es.shutdown();
        es.awaitTermination(30, TimeUnit.MINUTES);

        Throwable failure = firstFailure.get();
        if (failure != null) {
            System.out.println("REPRODUCED after ok=" + ok.get());
            failure.printStackTrace(System.out);
            System.exit(1);
        }
        System.out.println("clean ok=" + ok.get() + " threads=" + threads + " iters=" + iters);
    }
}
