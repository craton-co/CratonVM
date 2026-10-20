// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 74, lane L2 -- the Java-visible answers of the
// StackTraceElement[] behind Throwable.getStackTrace() / printStackTrace(),
// which CRATONVM_STACK_TRACE_ELEMENT_OF_INTRINSIC=1 (StackTraceElement.of
// built natively under --jdk-only, computeFormat() answered by the VM) must
// not move: toString() of application frames (built-in loader: no "app/"),
// java.base frames (hashed module: no "@version"), a java.sql frame (platform
// loader and a hashed module), a frame of a class defined by a NAMED user
// loader ("custom//"), a lambda body, the origin getters, printStackTrace()
// of a cause chain ("... 1 more"), a repeated site, the 1024-frame cap and a
// copy per call. Line numbers are stripped (":NN)" -> ")") so the block below
// does not depend on the JDK's sources; a repeated site whose traces differ
// prints both on stderr.
// Proposal: docs/internal/fixed-bugs/interpreter-i74-L2-proposal-stack-trace-element-of-intrinsic-20261107-DONE-20261107.md.
//
// Run each of (the stdout must be the block below, byte for byte):
//   $B --java-home $J/.. -cp out L2W74StackTraceElementShapes
//   env CRATONVM_STACK_TRACE_ELEMENT_OF_INTRINSIC=1 $B --java-home $J/.. -cp out L2W74StackTraceElementShapes
//   (each also with --compatible, where the switch registers nothing)
//   $J/java -cp out L2W74StackTraceElementShapes
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// shallow-0 L2W74StackTraceElementShapes.shallow(L2W74StackTraceElementShapes.java)
// shallow-1 L2W74StackTraceElementShapes.main(L2W74StackTraceElementShapes.java)
// shallow-origin app null null
// miss-0 java.base/jdk.internal.loader.BuiltinClassLoader.loadClass(BuiltinClassLoader.java)
// miss-1 java.base/java.lang.ClassLoader.loadClass(ClassLoader.java)
// miss-2 L2W74StackTraceElementShapes.platformMiss(L2W74StackTraceElementShapes.java)
// miss-3 L2W74StackTraceElementShapes.main(L2W74StackTraceElementShapes.java)
// miss-top-origin null java.base true
// sql-0 java.sql/java.sql.Timestamp.valueOf(Timestamp.java)
// sql-origin platform java.sql true
// custom-0 custom//L2W74StackTraceElementShapes$Thrower.get(L2W74StackTraceElementShapes.java)
// custom-origin custom null null
// custom-1 L2W74StackTraceElementShapes.main(L2W74StackTraceElementShapes.java)
// lambda-0 L2W74StackTraceElementShapes.lambda$main$0(L2W74StackTraceElementShapes.java)
// printed java.lang.RuntimeException: outer|at L2W74StackTraceElementShapes.main(L2W74StackTraceElementShapes.java)|Caused by: java.lang.Exception: shallow|at L2W74StackTraceElementShapes.shallow(L2W74StackTraceElementShapes.java)|... 1 more|
// shallow-repeat-equal true
// miss-repeat-equal true
// deep-length 1024
// copies-distinct true equal true
// HOTSPOT_EXPECTED_END
import java.io.PrintWriter;
import java.io.StringWriter;
import java.net.URI;
import java.net.URL;
import java.net.URLClassLoader;
import java.util.Arrays;
import java.util.function.Supplier;

public class L2W74StackTraceElementShapes {
    static String strip(String s) {
        return s.replaceAll(":[0-9]+\\)", ")");
    }

    static String el(StackTraceElement e) {
        return strip(e.toString());
    }

    static Exception shallow() {
        return new Exception("shallow");
    }

    static Exception platformMiss(ClassLoader loader, String name) {
        try {
            loader.loadClass(name);
            return null;
        } catch (ClassNotFoundException e) {
            return e;
        }
    }

    static int deep(int n) {
        if (n == 0) {
            return new Exception().getStackTrace().length;
        }
        return deep(n - 1);
    }

    /** Loaded a second time by a named URLClassLoader whose parent is the boot loader. */
    public static class Thrower implements Supplier<Exception> {
        public Thrower() {
        }

        @Override
        public Exception get() {
            return new Exception("thrower");
        }
    }

    static String printed(Throwable t) {
        StringWriter w = new StringWriter();
        t.printStackTrace(new PrintWriter(w));
        return strip(w.toString()).replace("\r", "").replace("\n", "|").replace("\t", "");
    }

    /** One call site repeated: every trace must equal the first, line numbers included. */
    static boolean repeatEqual(String what, Supplier<Exception> site, int n) {
        StackTraceElement[] first = null;
        for (int i = 0; i < n; i++) {
            StackTraceElement[] st = site.get().getStackTrace();
            if (first == null) {
                first = st;
            } else if (!Arrays.equals(first, st)) {
                System.err.println(what + " differs at repeat " + i);
                System.err.println("  first " + Arrays.toString(first));
                System.err.println("  now   " + Arrays.toString(st));
                return false;
            }
        }
        return true;
    }

    public static void main(String[] args) throws Exception {
        // The first lambda in this class: its body is lambda$main$0.
        Supplier<Exception> lambda = () -> new Exception("lambda");

        StackTraceElement[] s = shallow().getStackTrace();
        System.out.println("shallow-0 " + el(s[0]));
        System.out.println("shallow-1 " + el(s[1]));
        System.out.println("shallow-origin " + s[0].getClassLoaderName() + " " + s[0].getModuleName()
                + " " + s[0].getModuleVersion());

        ClassLoader platform = ClassLoader.getPlatformClassLoader();
        StackTraceElement[] m = platformMiss(platform, "l2w74.Missing").getStackTrace();
        for (int i = 0; i < m.length; i++) {
            System.out.println("miss-" + i + " " + el(m[i]));
        }
        System.out.println("miss-top-origin " + m[0].getClassLoaderName() + " " + m[0].getModuleName() + " "
                + (m[0].getModuleVersion() != null));

        StackTraceElement[] q;
        try {
            java.sql.Timestamp.valueOf((String) null);
            q = new StackTraceElement[0];
        } catch (IllegalArgumentException e) {
            q = e.getStackTrace();
        }
        System.out.println("sql-0 " + el(q[0]));
        System.out.println("sql-origin " + q[0].getClassLoaderName() + " " + q[0].getModuleName() + " "
                + (q[0].getModuleVersion() != null));

        URL self = L2W74StackTraceElementShapes.class.getResource("L2W74StackTraceElementShapes.class");
        String u = self.toString();
        URL dir = URI.create(u.substring(0, u.lastIndexOf('/') + 1)).toURL();
        try (URLClassLoader custom = new URLClassLoader("custom", new URL[] {dir}, null)) {
            @SuppressWarnings("unchecked")
            Supplier<Exception> thrower = (Supplier<Exception>) custom
                    .loadClass("L2W74StackTraceElementShapes$Thrower")
                    .getDeclaredConstructor()
                    .newInstance();
            StackTraceElement[] c = thrower.get().getStackTrace();
            System.out.println("custom-0 " + el(c[0]));
            System.out.println("custom-origin " + c[0].getClassLoaderName() + " " + c[0].getModuleName() + " "
                    + c[0].getModuleVersion());
            System.out.println("custom-1 " + el(c[1]));
        }

        System.out.println("lambda-0 " + el(lambda.get().getStackTrace()[0]));

        RuntimeException outer = new RuntimeException("outer", shallow());
        System.out.println("printed " + printed(outer));

        System.out.println("shallow-repeat-equal "
                + repeatEqual("shallow", L2W74StackTraceElementShapes::shallow, 300));
        System.out.println("miss-repeat-equal "
                + repeatEqual("miss", () -> platformMiss(platform, "l2w74.Missing"), 300));

        System.out.println("deep-length " + deep(1100));

        Exception one = shallow();
        StackTraceElement[] a = one.getStackTrace();
        StackTraceElement[] b = one.getStackTrace();
        System.out.println("copies-distinct " + (a != b) + " equal " + Arrays.equals(a, b));
    }
}
