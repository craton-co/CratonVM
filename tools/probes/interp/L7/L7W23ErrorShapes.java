// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 23, lane L7 (bug hunt): the shapes of VM-raised
// errors a program can observe.
//
//   clinitSoe   a StackOverflowError inside <clinit>: an Error is NOT wrapped
//               in ExceptionInInitializerError; the class is then erroneous
//               and the next use is NoClassDefFoundError "Could not initialize
//               class ..." whose cause is an ExceptionInInitializerError naming
//               the original error
//   finallySoe  recursion whose frames all run a `finally`: the SOE escapes,
//               every finally ran on the way out, the thread survives
//   oom         `new long[Integer.MAX_VALUE]` and `new Object[MAX - 1]`:
//               "Requested array size exceeds VM limit"; a large-but-legal
//               size: "Java heap space"
//   cycle       printStackTrace of a cause cycle and a suppressed cycle:
//               "[CIRCULAR REFERENCE: ...]" lines instead of recursing
//   cleaner     an Error thrown by a Cleaner action does not reach the program
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -Xmx256m -cp <dir> L7W23ErrorShapes
// (HotSpot: `java -Xmx256m -cp <dir> L7W23ErrorShapes`; the heap cap keeps
// the "Java heap space" row independent of the machine.)
//
// HotSpot 25 prints exactly:
//     clinitSoe first java.lang.StackOverflowError
//     clinitSoe second java.lang.NoClassDefFoundError: Could not initialize class L7W23ErrorShapes$Deep
//     clinitSoe cause java.lang.ExceptionInInitializerError: Exception java.lang.StackOverflowError [in thread "main"]
//     finallySoe java.lang.StackOverflowError finallies=all alive=true
//     oom long[MAX] java.lang.OutOfMemoryError: Requested array size exceeds VM limit
//     oom Object[MAX-1] java.lang.OutOfMemoryError: Requested array size exceeds VM limit
//     oom byte[MAX-8] java.lang.OutOfMemoryError: Java heap space
//     oom int[1<<30] java.lang.OutOfMemoryError: Java heap space
//     cycle java.lang.RuntimeException: a
//     cycle Suppressed: java.lang.IllegalStateException: s
//     cycle Caused by: [CIRCULAR REFERENCE: java.lang.RuntimeException: a]
//     cycle Caused by: java.lang.RuntimeException: b
//     cycle Caused by: [CIRCULAR REFERENCE: java.lang.RuntimeException: a]
//     cleaner ran=true survived=true
import java.io.ByteArrayOutputStream;
import java.io.PrintStream;
import java.lang.ref.Cleaner;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

public class L7W23ErrorShapes {
    static class Deep {
        static int depth;
        static final int VALUE = recurse(0);

        static int recurse(int n) {
            depth = n;
            return recurse(n + 1) + 1;
        }
    }

    static void clinitSoe() {
        try {
            System.out.println("clinitSoe first " + Deep.VALUE);
        } catch (Throwable t) {
            System.out.println("clinitSoe first " + t.getClass().getName());
        }
        try {
            System.out.println("clinitSoe second " + Deep.VALUE);
        } catch (Throwable t) {
            System.out.println("clinitSoe second " + t);
            System.out.println("clinitSoe cause " + t.getCause());
        }
    }

    static int entered;
    static int finished;

    static void deepFinally(int n) {
        entered++;
        try {
            deepFinally(n + 1);
        } finally {
            finished++;
        }
    }

    static void finallySoe() {
        entered = 0;
        finished = 0;
        String what;
        try {
            deepFinally(0);
            what = "none";
        } catch (StackOverflowError e) {
            what = e.getClass().getName();
        }
        // Every entered frame ran its finally (a finally that itself overflows
        // re-throws a fresh SOE, which still unwinds through the rest).
        System.out.println("finallySoe " + what + " finallies=" + (entered == finished ? "all" : entered + "/" + finished)
                + " alive=true");
    }

    static void oom(String name, Runnable r) {
        try {
            r.run();
            System.out.println("oom " + name + " allocated");
        } catch (OutOfMemoryError e) {
            System.out.println("oom " + name + " " + e);
        }
    }

    static Object sink;

    static void cycle() {
        RuntimeException a = new RuntimeException("a");
        RuntimeException b = new RuntimeException("b", a);
        a.initCause(b);
        a.addSuppressed(new IllegalStateException("s"));
        a.getSuppressed()[0].initCause(a);
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        a.printStackTrace(new PrintStream(bytes, true));
        for (String line : bytes.toString().split("\\R")) {
            String l = line.trim();
            if (l.startsWith("at ") || l.startsWith("...")) {
                continue;
            }
            System.out.println("cycle " + l);
        }
    }

    static void cleaner() throws Exception {
        Cleaner c = Cleaner.create();
        CountDownLatch ran = new CountDownLatch(1);
        Object o = new Object();
        c.register(o, () -> {
            ran.countDown();
            throw new Error("from cleaner");
        });
        o = null;
        boolean done = false;
        for (int i = 0; i < 200 && !done; i++) {
            System.gc();
            done = ran.await(50, TimeUnit.MILLISECONDS);
        }
        System.out.println("cleaner ran=" + done + " survived=true");
    }

    public static void main(String[] args) throws Exception {
        clinitSoe();
        finallySoe();
        oom("long[MAX]", () -> sink = new long[Integer.MAX_VALUE]);
        oom("Object[MAX-1]", () -> sink = new Object[Integer.MAX_VALUE - 1]);
        oom("byte[MAX-8]", () -> sink = new byte[Integer.MAX_VALUE - 8]);
        oom("int[1<<30]", () -> sink = new int[1 << 30]);
        sink = null;
        cycle();
        cleaner();
    }
}
