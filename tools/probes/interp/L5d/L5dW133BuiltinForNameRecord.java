// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// probe + bench
//
// Interpreter round i6 wave 133, lane L5d: `CRATONVM_BUILTIN_FOR_NAME_RECORD` (performance,
// default off). `Class.forName(name, init, loader)` with the application or the platform loader
// answers a name that loader already answered a `forName` for from the VM's per-loader record
// (HotSpot's `SystemDictionary` dictionary hit), without running `BuiltinClassLoader.loadClass`
// again. Page: docs/known-issues/interpreter/i133-L2-typevariable-getannotations-is-16-to-19x-hotspot-xint-20261231.md.
// The answers must not move, in either mode, with the switch on or off:
//
//   app-same       an application class through the application loader, 3 times: one class,
//                  initialize=false does not run <clinit>; initialize=true runs it once
//   lazy           a class first named by forName(false): loaded, not initialized; then
//                  forName(true) initializes it, a repeat does not again
//   jdk-app        java.util.Map / java.lang.String through the application loader, repeated
//   jdk-platform   the same through the platform loader; java.sql.Date (a platform-module class)
//                  through both loaders
//   one-arg        Class.forName(String) from an application class (initializes), repeated
//   array          an array descriptor through the application loader, repeated
//   missing        a missing name, 3 times: ClassNotFoundException each time, the same text
//   platform-app   an application class through the platform loader: ClassNotFoundException
//                  each time (the platform loader cannot see it, before or after the app loader
//                  answered it)
//   bad-init       forName(false) then forName(true) twice on a class whose <clinit> throws:
//                  ExceptionInInitializerError, then NoClassDefFoundError
//
// Bench (TIME lines): `forNameApp` and `forNameJdk` (expected down with the switch), `getName`
// flat (control).
//
// javac -d <dir> L5dW133BuiltinForNameRecord.java
// for m in --jdk-only --compatible; do for j in "" --nojit; do
//   cratonvm $m $j -cp <dir> L5dW133BuiltinForNameRecord
//   CRATONVM_BUILTIN_FOR_NAME_RECORD=1 cratonvm $m $j -cp <dir> L5dW133BuiltinForNameRecord
// done; done
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint; TIME lines excluded)
// app-same same=true initsAfterFalse=0 initsAfterTrue=1 loader=app
// lazy same=true initsAfterFalse=0 initsAfterTrue=1
// jdk-app same=true
// jdk-platform same=true
// sql-platform ok java.sql.Date loader=platform
// sql-app ok java.sql.Date loader=platform
// sql-platform ok java.sql.Date loader=platform
// sql-app ok java.sql.Date loader=platform
// sql-same=true
// one-arg same=true inits=1
// array same=true
// missing java.lang.ClassNotFoundException msg=nope.L5dW133Missing cause=null
// missing java.lang.ClassNotFoundException msg=nope.L5dW133Missing cause=null
// missing java.lang.ClassNotFoundException msg=nope.L5dW133Missing cause=null
// missing-init java.lang.ClassNotFoundException msg=nope.L5dW133Missing cause=null
// missing-init java.lang.ClassNotFoundException msg=nope.L5dW133Missing cause=null
// platform-app java.lang.ClassNotFoundException msg=L5dW133BuiltinForNameRecord$Hello cause=null
// platform-app java.lang.ClassNotFoundException msg=L5dW133BuiltinForNameRecord$Hello cause=null
// bad-init false ok L5dW133BuiltinForNameRecord$Bad loader=app
// bad-init false ok L5dW133BuiltinForNameRecord$Bad loader=app
// bad-init true java.lang.ExceptionInInitializerError msg=null cause=java.lang.RuntimeException
// bad-init true java.lang.NoClassDefFoundError msg=Could not initialize class L5dW133BuiltinForNameRecord$Bad cause=java.lang.ExceptionInInitializerError
// bad-init false-after ok L5dW133BuiltinForNameRecord$Bad loader=app
// HOTSPOT_EXPECTED_END
// HotSpot TIME on this host (ns/op): forNameApp 860-900 default, 780-815 -Xint; forNameJdk
// 480-490 default, 445-480 -Xint; getName 90-135.
public class L5dW133BuiltinForNameRecord {
    static int helloInits;
    static int lazyInits;

    static class Hello {
        static {
            helloInits++;
        }
    }

    static class Lazy {
        static {
            lazyInits++;
        }
    }

    static class OneArg {
        static int inits;

        static {
            inits++;
        }
    }

    static class Bad {
        static {
            if (Boolean.TRUE) {
                throw new RuntimeException("boom");
            }
        }
    }

    static String describe(Throwable t) {
        Throwable c = t.getCause();
        return t.getClass().getName() + " msg=" + t.getMessage()
                + " cause=" + (c == null ? "null" : c.getClass().getName());
    }

    static String tryForName(String name, boolean init, ClassLoader loader) {
        try {
            Class<?> c = Class.forName(name, init, loader);
            return "ok " + c.getName() + " loader=" + loaderName(c.getClassLoader());
        } catch (Throwable t) {
            return describe(t);
        }
    }

    static String loaderName(ClassLoader l) {
        if (l == null) return "boot";
        if (l == ClassLoader.getSystemClassLoader()) return "app";
        if (l == ClassLoader.getPlatformClassLoader()) return "platform";
        return l.getClass().getName();
    }

    public static void main(String[] args) throws Exception {
        ClassLoader app = ClassLoader.getSystemClassLoader();
        ClassLoader platform = ClassLoader.getPlatformClassLoader();
        String hello = L5dW133BuiltinForNameRecord.class.getName() + "$Hello";

        Class<?> h1 = Class.forName(hello, false, app);
        Class<?> h2 = Class.forName(hello, false, app);
        int initsAfterFalse = helloInits;
        Class<?> h3 = Class.forName(hello, true, app);
        Class<?> h4 = Class.forName(hello, true, app);
        System.out.println("app-same same=" + (h1 == h2 && h2 == h3 && h3 == h4 && h1 == Hello.class)
                + " initsAfterFalse=" + initsAfterFalse + " initsAfterTrue=" + helloInits
                + " loader=" + loaderName(h1.getClassLoader()));

        String lazy = L5dW133BuiltinForNameRecord.class.getName() + "$Lazy";
        Class<?> l1 = Class.forName(lazy, false, app);
        Class<?> l2 = Class.forName(lazy, false, app);
        int lazyAfterFalse = lazyInits;
        Class<?> l3 = Class.forName(lazy, true, app);
        Class<?> l4 = Class.forName(lazy, true, app);
        System.out.println("lazy same=" + (l1 == l2 && l2 == l3 && l3 == l4)
                + " initsAfterFalse=" + lazyAfterFalse + " initsAfterTrue=" + lazyInits);

        boolean jdkApp = true;
        for (int i = 0; i < 3; i++) {
            jdkApp &= Class.forName("java.util.Map", false, app) == java.util.Map.class;
            jdkApp &= Class.forName("java.lang.String", true, app) == String.class;
        }
        System.out.println("jdk-app same=" + jdkApp);

        boolean jdkPlatform = true;
        for (int i = 0; i < 3; i++) {
            jdkPlatform &= Class.forName("java.util.Map", false, platform) == java.util.Map.class;
            jdkPlatform &= Class.forName("java.lang.String", true, platform) == String.class;
        }
        System.out.println("jdk-platform same=" + jdkPlatform);
        for (int i = 0; i < 2; i++) {
            System.out.println("sql-platform " + tryForName("java.sql.Date", false, platform));
            System.out.println("sql-app " + tryForName("java.sql.Date", false, app));
        }
        System.out.println("sql-same=" + (Class.forName("java.sql.Date", false, platform)
                == Class.forName("java.sql.Date", true, app)));

        String oneArg = L5dW133BuiltinForNameRecord.class.getName() + "$OneArg";
        Class<?> o1 = Class.forName(oneArg);
        Class<?> o2 = Class.forName(oneArg);
        System.out.println("one-arg same=" + (o1 == o2 && o1 == OneArg.class) + " inits=" + OneArg.inits);

        Class<?> a1 = Class.forName("[Ljava.lang.String;", false, app);
        Class<?> a2 = Class.forName("[Ljava.lang.String;", false, app);
        System.out.println("array same=" + (a1 == a2 && a1 == String[].class));

        for (int i = 0; i < 3; i++) {
            System.out.println("missing " + tryForName("nope.L5dW133Missing", false, app));
        }
        for (int i = 0; i < 2; i++) {
            System.out.println("missing-init " + tryForName("nope.L5dW133Missing", true, app));
        }

        for (int i = 0; i < 2; i++) {
            System.out.println("platform-app " + tryForName(hello, false, platform));
        }

        String bad = L5dW133BuiltinForNameRecord.class.getName() + "$Bad";
        System.out.println("bad-init false " + tryForName(bad, false, app));
        System.out.println("bad-init false " + tryForName(bad, false, app));
        System.out.println("bad-init true " + tryForName(bad, true, app));
        System.out.println("bad-init true " + tryForName(bad, true, app));
        System.out.println("bad-init false-after " + tryForName(bad, false, app));

        bench(app);
    }

    static volatile Object sink;

    static void bench(ClassLoader app) throws Exception {
        int n = 2000;
        String hello = L5dW133BuiltinForNameRecord.class.getName() + "$Hello";
        for (int rep = 0; rep < 3; rep++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < n; i++) sink = Class.forName(hello, false, app);
            long t1 = System.nanoTime();
            for (int i = 0; i < n; i++) sink = Class.forName("java.util.Map", false, app);
            long t2 = System.nanoTime();
            for (int i = 0; i < n; i++) sink = Hello.class.getName();
            long t3 = System.nanoTime();
            System.out.println("TIME forNameApp " + (t1 - t0) / n + " ns/op");
            System.out.println("TIME forNameJdk " + (t2 - t1) / n + " ns/op");
            System.out.println("TIME getName " + (t3 - t2) / n + " ns/op");
        }
    }
}
