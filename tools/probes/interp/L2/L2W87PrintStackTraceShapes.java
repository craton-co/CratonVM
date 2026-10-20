// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 87, lane L2 -- the text Throwable.printStackTrace
// prints (`Throwable.printEnclosedStackTrace`), in both modes:
//
//   chain       "Caused by:" with the "... N more" elision of frames in
//               common with the enclosing trace (counted from the bottom,
//               compared as StackTraceElements, line numbers included)
//   suppressed  "Suppressed:" one tab deeper, its own "... N more"
//   cycle       a cause cycle ends in "[CIRCULAR REFERENCE: <toString>]"
//   tostring    a toString() that throws: nothing printed, the exception
//               leaves printStackTrace; as a CAUSE, the enclosing lines are
//               printed first
//   overrides   a subclass's getCause() override is followed (virtual), a
//               getStackTrace() override is NOT (getOurStackTrace is private)
//   native      a frame of a native method reads "(Native Method)"
//   proxy       a frame of a java.lang.reflect.Proxy class: dynamic module
//               prefix, "(Unknown Source)" (no SourceFile attribute)
//   empty       a throwable with no frames prints its header alone
//
// Line numbers are cut (":NN)" -> ")"), a tab is shown as "<TAB>", and the
// proxy's numbers are normalised (jdk.proxyN / $ProxyN). The lines are
// printed with a "| " prefix.
//
// Run (stdout must be the block below, byte for byte):
//   $J/javac -d out tools/probes/interp/L2/L2W87PrintStackTraceShapes.java
//   $B --java-home $J/.. -cp out L2W87PrintStackTraceShapes
//   $B --compatible --java-home $J/.. -cp out L2W87PrintStackTraceShapes
//   (each also with --nojit)
//   $J/java -cp out L2W87PrintStackTraceShapes
//
// HOTSPOT_EXPECTED_BEGIN
// == chain
// | java.lang.RuntimeException: outer
// | <TAB>at L2W87PrintStackTraceShapes.chain(L2W87PrintStackTraceShapes.java)
// | <TAB>at L2W87PrintStackTraceShapes.main(L2W87PrintStackTraceShapes.java)
// | Caused by: java.io.IOException: inner
// | <TAB>at L2W87PrintStackTraceShapes.level2(L2W87PrintStackTraceShapes.java)
// | <TAB>at L2W87PrintStackTraceShapes.level1(L2W87PrintStackTraceShapes.java)
// | <TAB>at L2W87PrintStackTraceShapes.chain(L2W87PrintStackTraceShapes.java)
// | <TAB>... 1 more
// == suppressed
// | java.lang.Exception: primary
// | <TAB>at L2W87PrintStackTraceShapes.withSuppressed(L2W87PrintStackTraceShapes.java)
// | <TAB>at L2W87PrintStackTraceShapes.main(L2W87PrintStackTraceShapes.java)
// | <TAB>Suppressed: java.lang.IllegalStateException: s1
// | <TAB><TAB>at L2W87PrintStackTraceShapes.suppressedOne(L2W87PrintStackTraceShapes.java)
// | <TAB><TAB>at L2W87PrintStackTraceShapes.withSuppressed(L2W87PrintStackTraceShapes.java)
// | <TAB><TAB>... 1 more
// == cycle
// | java.lang.Exception: a
// | <TAB>at L2W87PrintStackTraceShapes.cycle(L2W87PrintStackTraceShapes.java)
// | <TAB>at L2W87PrintStackTraceShapes.main(L2W87PrintStackTraceShapes.java)
// | Caused by: java.lang.Exception: b
// | <TAB>at L2W87PrintStackTraceShapes.cycle(L2W87PrintStackTraceShapes.java)
// | <TAB>... 1 more
// | Caused by: [CIRCULAR REFERENCE: java.lang.Exception: a]
// == tostring
// threw java.lang.IllegalStateException: bad toString
// == tostring-cause
// | java.lang.RuntimeException: wraps
// | <TAB>at L2W87PrintStackTraceShapes.wrapsBad(L2W87PrintStackTraceShapes.java)
// | <TAB>at L2W87PrintStackTraceShapes.main(L2W87PrintStackTraceShapes.java)
// threw java.lang.IllegalStateException: bad toString
// == overrides
// | L2W87PrintStackTraceShapes$Odd: odd
// | <TAB>at L2W87PrintStackTraceShapes.overrides(L2W87PrintStackTraceShapes.java)
// | <TAB>at L2W87PrintStackTraceShapes.main(L2W87PrintStackTraceShapes.java)
// | Caused by: java.lang.Error: virtual cause
// | <TAB>at L2W87PrintStackTraceShapes.overrides(L2W87PrintStackTraceShapes.java)
// | <TAB>... 1 more
// == native
// native-frames 1 first-native-is-top true
// == proxy
// proxy-frame jdk.proxyN/jdk.proxyN.$ProxyN.run(Unknown Source)
// == empty
// | java.lang.Exception: frameless
// HOTSPOT_EXPECTED_END
import java.io.IOException;
import java.io.PrintWriter;
import java.io.StringWriter;
import java.lang.reflect.Proxy;

public class L2W87PrintStackTraceShapes {
    static String render(Throwable t) {
        StringWriter sw = new StringWriter();
        PrintWriter pw = new PrintWriter(sw, true);
        try {
            t.printStackTrace(pw);
        } catch (RuntimeException e) {
            pw.flush();
            dump(sw.toString());
            return "threw " + e;
        }
        pw.flush();
        dump(sw.toString());
        return null;
    }

    static String norm(String line) {
        return line.replaceAll(":\\d+\\)", ")").replace("\t", "<TAB>");
    }

    static void dump(String text) {
        text.lines().forEach(l -> System.out.println("| " + norm(l)));
    }

    static void section(String name, Throwable t) {
        System.out.println("== " + name);
        String threw = render(t);
        if (threw != null) {
            System.out.println(threw);
        }
    }

    static Throwable chain() {
        try {
            level1();
            return null;
        } catch (Exception e) {
            return new RuntimeException("outer", e);
        }
    }

    static void level1() throws Exception {
        level2();
    }

    static void level2() throws Exception {
        throw new IOException("inner");
    }

    static Exception withSuppressed() {
        Exception primary = new Exception("primary");
        primary.addSuppressed(suppressedOne());
        return primary;
    }

    static Exception suppressedOne() {
        return new IllegalStateException("s1");
    }

    static Exception cycle() {
        Exception a = new Exception("a");
        Exception b = new Exception("b", a);
        a.initCause(b);
        return a;
    }

    static final class Bad extends RuntimeException {
        Bad() {
            super("bad");
        }

        @Override
        public String toString() {
            throw new IllegalStateException("bad toString");
        }
    }

    static RuntimeException wrapsBad() {
        return new RuntimeException("wraps", new Bad());
    }

    static final class Odd extends Exception {
        final Error virtualCause;

        Odd(Error virtualCause) {
            super("odd");
            this.virtualCause = virtualCause;
        }

        @Override
        public synchronized Throwable getCause() {
            return virtualCause;
        }

        @Override
        public StackTraceElement[] getStackTrace() {
            return new StackTraceElement[] {new StackTraceElement("Fake", "never", "Fake.java", 1)};
        }
    }

    static Odd overrides() {
        Error cause = new Error("virtual cause");
        return new Odd(cause);
    }

    public static void main(String[] args) throws Exception {
        section("chain", chain());
        section("suppressed", withSuppressed());
        section("cycle", cycle());
        System.out.println("== tostring");
        System.out.println(render(new Bad()));
        section("tostring-cause", wrapsBad());
        section("overrides", overrides());

        System.out.println("== native");
        Thread.currentThread().interrupt();
        try {
            Thread.sleep(1000);
        } catch (InterruptedException e) {
            StackTraceElement[] st = e.getStackTrace();
            int natives = 0;
            for (StackTraceElement el : st) {
                if (el.isNativeMethod() && el.toString().endsWith("(Native Method)")) {
                    natives++;
                }
            }
            System.out.println("native-frames " + natives + " first-native-is-top "
                    + (st.length > 0 && st[0].isNativeMethod()));
        }

        System.out.println("== proxy");
        Runnable proxy = (Runnable) Proxy.newProxyInstance(L2W87PrintStackTraceShapes.class.getClassLoader(),
                new Class<?>[] {Runnable.class}, (p, m, a) -> {
                    throw new IllegalArgumentException("from handler");
                });
        try {
            proxy.run();
        } catch (IllegalArgumentException e) {
            String frame = "none";
            for (StackTraceElement el : e.getStackTrace()) {
                if (el.getClassName().contains("$Proxy")) {
                    frame = el.toString().replaceAll("proxy\\d+", "proxyN").replaceAll("\\$Proxy\\d+", "\\$ProxyN");
                    break;
                }
            }
            System.out.println("proxy-frame " + frame);
        }

        Exception frameless = new Exception("frameless");
        frameless.setStackTrace(new StackTraceElement[0]);
        section("empty", frameless);
    }
}
