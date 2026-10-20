// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 73, lane L2 -- the Java-visible answers a
// throwable's capture gives, which CRATONVM_THROWABLE_LOADER_MISS_MEMO=1 (a
// class-loader delegation miss's capture taken from the capture memo) must not
// move: getStackTrace() of a shallow, a five-frame and a delegation-miss
// throwable (platform loader and a bare URLClassLoader), repeated at one site
// and at two sites of one method (a memo answering for the wrong call site
// would make them equal), fillInStackTrace overrides that return early or call
// super, an unwritable trace, suppression on and off, a refill, the
// MaxJavaStackTraceDepth cap, StackWalker walks and getCallerClass.
// Proposal: docs/internal/fixed-bugs/interpreter-i73-L2-proposal-memoise-the-delegation-miss-capture-KEPT-OFF-20261106.md.
//
// Run each of (the output must be the block below, byte for byte):
//   $B --java-home $J/.. -cp out L2W73ThrowableTraceShapes
//   env CRATONVM_THROWABLE_LOADER_MISS_MEMO=1 $B --java-home $J/.. -cp out L2W73ThrowableTraceShapes
//   (each also with --compatible)
//   $J/java -cp out L2W73ThrowableTraceShapes
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// shallow L2W73ThrowableTraceShapes.shallow L2W73ThrowableTraceShapes.viaB L2W73ThrowableTraceShapes.main
// through-five L2W73ThrowableTraceShapes.t0 L2W73ThrowableTraceShapes.t1 L2W73ThrowableTraceShapes.t2 L2W73ThrowableTraceShapes.t3 L2W73ThrowableTraceShapes.t4 L2W73ThrowableTraceShapes.throughFive L2W73ThrowableTraceShapes.main
// through-five-line true
// platform-miss java.lang.ClassNotFoundException l2w73.Missing0
// platform-miss-trace jdk.internal.loader.BuiltinClassLoader.loadClass java.lang.ClassLoader.loadClass L2W73ThrowableTraceShapes.platformMiss L2W73ThrowableTraceShapes.main
// platform-miss-cause null
// platform-miss-top java.base BuiltinClassLoader.java true
// platform-miss-caller-loader app
// platform-miss-repeat-equal true
// two-sites-differ true
// two-sites-stable true
// url-miss java.lang.ClassNotFoundException l2w73.Missing0
// url-miss-trace java.net.URLClassLoader.findClass java.lang.ClassLoader.loadClass java.lang.ClassLoader.loadClass L2W73ThrowableTraceShapes.urlMiss L2W73ThrowableTraceShapes.main
// url-miss-suppressed 0
// url-miss-repeat-equal true
// quiet-override-depth 0
// loud-override L2W73ThrowableTraceShapes.makeLoud L2W73ThrowableTraceShapes.main
// unwritable-depth 0 0
// suppression-enabled 1
// suppression-disabled 0
// refill L2W73ThrowableTraceShapes.refill L2W73ThrowableTraceShapes.main
// deep-length 1024
// walk L2W73ThrowableTraceShapes.walkHere L2W73ThrowableTraceShapes.walkTwo L2W73ThrowableTraceShapes.main
// caller-class L2W73ThrowableTraceShapes$Asker
// walk-in-ctor <init> makeWalking main
// clone-distinct true equal true
// HOTSPOT_EXPECTED_END
import java.net.URL;
import java.net.URLClassLoader;
import java.util.Arrays;
import java.util.List;
import java.util.stream.Collectors;

public class L2W73ThrowableTraceShapes {
    static String names(StackTraceElement[] st) {
        StringBuilder sb = new StringBuilder();
        for (StackTraceElement e : st) {
            if (sb.length() > 0) {
                sb.append(' ');
            }
            sb.append(e.getClassName()).append('.').append(e.getMethodName());
        }
        return sb.toString();
    }

    static Exception shallow() {
        return new Exception("shallow");
    }

    static Exception viaB() {
        return shallow();
    }

    static void t0() throws Exception {
        throw new Exception("t0");
    }

    static void t1() throws Exception {
        t0();
    }

    static void t2() throws Exception {
        t1();
    }

    static void t3() throws Exception {
        t2();
    }

    static void t4() throws Exception {
        t3();
    }

    static Exception throughFive() {
        try {
            t4();
            return null;
        } catch (Exception e) {
            return e;
        }
    }

    static Exception platformMiss(ClassLoader loader, String name) {
        try {
            loader.loadClass(name);
            return null;
        } catch (ClassNotFoundException e) {
            return e;
        }
    }

    static Exception urlMiss(ClassLoader loader, String name) {
        try {
            loader.loadClass(name);
            return null;
        } catch (ClassNotFoundException e) {
            return e;
        }
    }

    /** Two delegation misses from two call sites of one method. */
    static Exception[] twoSites(ClassLoader loader) {
        Exception first = platformMiss(loader, "l2w73.SiteA");
        Exception second = platformMiss(loader, "l2w73.SiteB");
        return new Exception[] {first, second};
    }

    static class Quiet extends Exception {
        Quiet() {
            super("quiet");
        }

        @Override
        public synchronized Throwable fillInStackTrace() {
            return this;
        }
    }

    static class Loud extends Exception {
        @Override
        public synchronized Throwable fillInStackTrace() {
            return super.fillInStackTrace();
        }
    }

    /** The protected four-argument constructor. */
    static class Custom extends Exception {
        Custom(String message, boolean enableSuppression, boolean writableStackTrace) {
            super(message, null, enableSuppression, writableStackTrace);
        }
    }

    static Loud makeLoud() {
        return new Loud();
    }

    static void refill(Exception e) {
        e.fillInStackTrace();
    }

    static int deepLength(int n) {
        if (n == 0) {
            return new Exception().getStackTrace().length;
        }
        return deepLength(n - 1);
    }

    static List<String> walkHere() {
        return StackWalker.getInstance()
                .walk(s -> s.map(f -> f.getClassName() + "." + f.getMethodName())
                        .collect(Collectors.toList()));
    }

    static List<String> walkTwo() {
        return walkHere();
    }

    static Class<?> callerOf() {
        return StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE).getCallerClass();
    }

    static class Asker {
        static Class<?> ask() {
            return callerOf();
        }
    }

    static class Walking extends Exception {
        final List<String> seen;

        Walking() {
            seen = StackWalker.getInstance()
                    .walk(s -> s.map(StackWalker.StackFrame::getMethodName).collect(Collectors.toList()));
        }
    }

    static Walking makeWalking() {
        return new Walking();
    }

    public static void main(String[] args) throws Exception {
        System.out.println("shallow " + names(viaB().getStackTrace()));
        Exception five = throughFive();
        System.out.println("through-five " + names(five.getStackTrace()));
        System.out.println("through-five-line " + (five.getStackTrace()[0].getLineNumber() > 0));

        ClassLoader platform = ClassLoader.getPlatformClassLoader();
        Exception miss = platformMiss(platform, "l2w73.Missing0");
        System.out.println("platform-miss " + miss.getClass().getName() + " " + miss.getMessage());
        StackTraceElement[] missTrace = miss.getStackTrace();
        System.out.println("platform-miss-trace " + names(missTrace));
        System.out.println("platform-miss-cause " + miss.getCause());
        StackTraceElement top = missTrace[0];
        System.out.println("platform-miss-top " + top.getModuleName() + " " + top.getFileName() + " "
                + (top.getLineNumber() > 0));
        System.out.println("platform-miss-caller-loader " + missTrace[2].getClassLoaderName());
        // One call site, so every trace must equal the first (frames and lines).
        boolean same = true;
        StackTraceElement[] first = null;
        for (int i = 0; i < 300; i++) {
            StackTraceElement[] st = platformMiss(platform, "l2w73.Missing" + (i % 7)).getStackTrace();
            if (first == null) {
                first = st;
            } else {
                same &= Arrays.equals(first, st);
            }
        }
        System.out.println("platform-miss-repeat-equal " + same);

        StackTraceElement[] siteA = null;
        StackTraceElement[] siteB = null;
        boolean differ = true;
        boolean stable = true;
        for (int i = 0; i < 300; i++) {
            Exception[] again = twoSites(platform);
            StackTraceElement[] a = again[0].getStackTrace();
            StackTraceElement[] b = again[1].getStackTrace();
            differ &= !Arrays.equals(a, b);
            if (siteA == null) {
                siteA = a;
                siteB = b;
            } else {
                stable &= Arrays.equals(siteA, a) && Arrays.equals(siteB, b);
            }
        }
        System.out.println("two-sites-differ " + differ);
        System.out.println("two-sites-stable " + stable);

        URLClassLoader url = new URLClassLoader(new URL[0], platform);
        Exception urlMissed = urlMiss(url, "l2w73.Missing0");
        System.out.println("url-miss " + urlMissed.getClass().getName() + " " + urlMissed.getMessage());
        StackTraceElement[] urlTrace = urlMissed.getStackTrace();
        System.out.println("url-miss-trace " + names(urlTrace));
        System.out.println("url-miss-suppressed " + urlMissed.getSuppressed().length);
        boolean urlSame = true;
        StackTraceElement[] urlFirst = null;
        for (int i = 0; i < 100; i++) {
            StackTraceElement[] st = urlMiss(url, "l2w73.Missing" + (i % 5)).getStackTrace();
            if (urlFirst == null) {
                urlFirst = st;
            } else {
                urlSame &= Arrays.equals(urlFirst, st);
            }
        }
        System.out.println("url-miss-repeat-equal " + urlSame);

        System.out.println("quiet-override-depth " + new Quiet().getStackTrace().length);
        System.out.println("loud-override " + names(makeLoud().getStackTrace()));

        Exception unwritable = new Custom("unwritable", true, false);
        int before = unwritable.getStackTrace().length;
        unwritable.setStackTrace(new StackTraceElement[] {
            new StackTraceElement("x.Y", "z", "Y.java", 1)
        });
        System.out.println("unwritable-depth " + before + " " + unwritable.getStackTrace().length);

        Exception enabled = new Exception("enabled");
        enabled.addSuppressed(new RuntimeException("s"));
        System.out.println("suppression-enabled " + enabled.getSuppressed().length);
        Exception disabled = new Custom("disabled", false, true);
        disabled.addSuppressed(new RuntimeException("s"));
        System.out.println("suppression-disabled " + disabled.getSuppressed().length);

        Exception refilled = shallow();
        refilled.getStackTrace();
        refill(refilled);
        System.out.println("refill " + names(refilled.getStackTrace()));

        System.out.println("deep-length " + deepLength(1100));

        System.out.println("walk " + String.join(" ", walkTwo()));
        System.out.println("caller-class " + Asker.ask().getName());
        System.out.println("walk-in-ctor " + String.join(" ", makeWalking().seen));

        Exception cloned = shallow();
        StackTraceElement[] c1 = cloned.getStackTrace();
        StackTraceElement[] c2 = cloned.getStackTrace();
        System.out.println("clone-distinct " + (c1 != c2) + " equal " + Arrays.equals(c1, c2));
    }
}
