// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 85, lane L2 -- `-XX:-StackTraceInThrowable`
// (`VmConfig::stack_trace_in_throwable`), which the launcher used to drop in
// its silent `-XX:` catch-all. HotSpot's `fill_in_stack_trace` returns at once
// under it: every throwable's `getStackTrace()` is empty, `printStackTrace()`
// prints the header line alone, and an implicit NPE has no helpful message
// (`getExtendedNPEMessage` reads the backtrace's top frame, and there is none).
// `StackWalker` is unaffected; the CURRENT thread's `Thread.getStackTrace()`
// is `new Exception().getStackTrace()` in JDK 25, so it is empty too.
//
// The explicit row loops 3000 times (so the JIT compiles it, when on) and
// prints the first run's answer and whether every run agreed. The implicit
// rows (NPE, ArithmeticException) print their FIRST throw only: a hot C2 site
// throws HotSpot's preallocated, stackless exception under the default
// OmitStackTraceInFastThrow, which would move an ON-mode `stable`.
//
// Run (stdout must be the block for the run, byte for byte):
//   $J/javac -d out tools/probes/interp/L2/L2W85StackTraceInThrowable.java
//   $B --java-home $J/.. -cp out L2W85StackTraceInThrowable                               -> ON
//   $B -XX:-StackTraceInThrowable --java-home $J/.. -cp out L2W85StackTraceInThrowable off -> OFF
//   (each also with --compatible and --nojit)
//   $J/java -cp out L2W85StackTraceInThrowable                                            -> ON
//   $J/java -XX:-StackTraceInThrowable -cp out L2W85StackTraceInThrowable off             -> OFF
//
// HOTSPOT_EXPECTED_BEGIN ON (JDK 25, default flags)
// explicit-depth nonzero stable=true
// npe-depth nonzero
// npe-message Cannot read field "v" because "<parameter1>" is null
// arith-depth nonzero
// print-lines 2+
// thread-trace nonzero
// walker nonzero
// HOTSPOT_EXPECTED_END
//
// HOTSPOT_EXPECTED_BEGIN OFF (-XX:-StackTraceInThrowable)
// explicit-depth 0 stable=true
// npe-depth 0
// npe-message null
// arith-depth 0
// print-lines 1
// thread-trace 0
// walker nonzero
// HOTSPOT_EXPECTED_END
import java.io.PrintWriter;
import java.io.StringWriter;

public class L2W85StackTraceInThrowable {
    static final class Node {
        int v;
    }

    interface Site {
        Throwable raise();
    }

    static int field(Node n) {
        return n.v;
    }

    static int div(int a, int b) {
        return a / b;
    }

    static String depth(Throwable t) {
        return t.getStackTrace().length == 0 ? "0" : "nonzero";
    }

    static void row(String label, Site site) {
        String first = null;
        boolean stable = true;
        for (int i = 0; i < 3000; i++) {
            String d = depth(site.raise());
            if (first == null) {
                first = d;
            } else if (!first.equals(d)) {
                stable = false;
            }
        }
        System.out.println(label + " " + first + " stable=" + stable);
    }

    static void once(String label, Site site) {
        System.out.println(label + " " + depth(site.raise()));
    }

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "on";
        row("explicit-depth", () -> new RuntimeException("x"));
        once("npe-depth", () -> {
            try {
                field(null);
                return new Error("no-throw");
            } catch (NullPointerException e) {
                return e;
            }
        });
        try {
            field(null);
        } catch (NullPointerException e) {
            System.out.println("npe-message " + e.getMessage());
        }
        once("arith-depth", () -> {
            try {
                div(1, 0);
                return new Error("no-throw");
            } catch (ArithmeticException e) {
                return e;
            }
        });
        StringWriter sw = new StringWriter();
        new IllegalStateException("printed").printStackTrace(new PrintWriter(sw, true));
        long lines = sw.toString().lines().count();
        System.out.println("print-lines " + (lines == 1 ? "1" : "2+"));
        System.out.println("thread-trace " + (Thread.currentThread().getStackTrace().length == 0 ? "0" : "nonzero"));
        long walked = StackWalker.getInstance().walk(s -> s.count());
        System.out.println("walker " + (walked == 0 ? "0" : "nonzero"));
        if (!mode.equals("on") && !mode.equals("off")) {
            System.out.println("unknown mode " + mode);
        }
    }
}
