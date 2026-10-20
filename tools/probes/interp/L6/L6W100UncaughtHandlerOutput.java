// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 100, lane L6 -- the default uncaught-exception
// output, as HotSpot 25 prints it through `ThreadGroup.uncaughtException`
// (`System.err.print("Exception in thread \"" + t.getName() + "\" ");
// e.printStackTrace(System.err)`), on platform and virtual threads. The
// output goes to `System.err`, which the probe replaces with a buffer, so a
// VM that prints the line natively (not through `System.err`) shows up as a
// missing line. Stack-frame lines ("\tat ...", "\t... n more") are dropped;
// everything else is printed with the case's label.
//
// Cases: a platform thread (message), a platform thread whose group is a
// subclass that does not override uncaughtException, an unnamed virtual
// thread (no message; JDK 25's `VirtualThread.run` dispatches through
// `dispatchUncaughtException` to the VirtualThreads group), a named virtual
// thread with a cause, the default handler set (it wins over the group), and
// a thread renamed while running (the name at the time of the print).
//
// HOTSPOT_EXPECTED_BEGIN
// platform: Exception in thread "w100-boom" java.lang.IllegalStateException: boom
// subgroup: Exception in thread "w100-sub" java.lang.UnsupportedOperationException: sub
// virtual-unnamed: Exception in thread "" java.lang.IllegalArgumentException
// virtual-named: Exception in thread "w100-vt" java.lang.RuntimeException: outer
// virtual-named: Caused by: java.lang.ArithmeticException: inner
// default-handler: handler saw w100-dflt java.lang.Error: dflt
// renamed: Exception in thread "w100-renamed" java.lang.IllegalStateException: late
// done
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayOutputStream;
import java.io.PrintStream;

public class L6W100UncaughtHandlerOutput {
    static void run(String label, Thread t) throws Exception {
        PrintStream saved = System.err;
        ByteArrayOutputStream buf = new ByteArrayOutputStream();
        PrintStream capture = new PrintStream(buf, true, "UTF-8");
        System.setErr(capture);
        try {
            t.start();
            t.join();
        } finally {
            System.setErr(saved);
        }
        capture.flush();
        for (String line : buf.toString("UTF-8").split("\\R")) {
            if (line.isEmpty() || line.startsWith("\t")) {
                continue;
            }
            System.out.println(label + ": " + line);
        }
    }

    public static void main(String[] args) throws Exception {
        run("platform", new Thread(() -> {
            throw new IllegalStateException("boom");
        }, "w100-boom"));

        ThreadGroup sub = new ThreadGroup("w100-group") { };
        run("subgroup", new Thread(sub, () -> {
            throw new UnsupportedOperationException("sub");
        }, "w100-sub"));

        run("virtual-unnamed", Thread.ofVirtual().unstarted(() -> {
            throw new IllegalArgumentException();
        }));

        run("virtual-named", Thread.ofVirtual().name("w100-vt").unstarted(() -> {
            throw new RuntimeException("outer", new ArithmeticException("inner"));
        }));

        Thread.UncaughtExceptionHandler before = Thread.getDefaultUncaughtExceptionHandler();
        Thread.setDefaultUncaughtExceptionHandler((th, e) ->
            System.err.println("handler saw " + th.getName() + " " + e));
        try {
            run("default-handler", new Thread(() -> {
                throw new Error("dflt");
            }, "w100-dflt"));
        } finally {
            Thread.setDefaultUncaughtExceptionHandler(before);
        }

        run("renamed", new Thread(() -> {
            Thread.currentThread().setName("w100-renamed");
            throw new IllegalStateException("late");
        }, "w100-before"));
        System.out.println("done");
    }
}
