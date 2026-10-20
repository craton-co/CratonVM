// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 103, lane L5 (follow-up of
// docs/known-issues/interpreter/i103-L2-compatible-self-reference-resolves-another-loaders-copy-20261205.md).
//
// The wider shape of L2W103RetiredBodyEscape's `direct` defect: an
// APPLICATION class `A` names ANOTHER application class `B`, and user loaders
// have defined their own copies of `B` (and of `A`) from the same bytes. JVMS
// 5.3: `A`'s references to `B` resolve through `A`'s defining loader (the
// application loader), so they always reach the application `B` -- its
// statics, its static method -- never a user loader's copy. A copy of `A`
// defined by a child-first loader that also defines `B` reaches THAT loader's
// `B`. The rows read and write statics from bytecode (`getstatic` /
// `putstatic` / `invokestatic` in `A.run`) and through reflection, so a wrong
// answer of either path shows: `tag` / `counter` of the wrong copy.
//
// `C` is first resolved by the application `A2` only AFTER a user loader
// defined its own `C`: the application class must still get the
// application loader's `C` (a fresh class, `tag=app`).
//
// Run (no setup): javac -d out L5W103CrossLoaderSameName.java
//   java -cp out L5W103CrossLoaderSameName
//   cratonvm [--compatible] [--nojit] -cp out L5W103CrossLoaderSameName
//
// Expected HotSpot 25 output:
//   before tag=app counter=11 bump=11 appLoader=true
//   copy1 app[tag=app counter=11 bump=11 appLoader=true] copy[tag=copy1 counter=1000] copyBump=1001
//   copy2 app[tag=app counter=11 bump=11 appLoader=true] copy[tag=copy2 counter=2000] copyBump=2001
//   copy3 app[tag=app counter=11 bump=11 appLoader=true] copy[tag=copy3 counter=3000] copyBump=3001
//   childFirst tag=app counter=11 bump=11 appLoader=false sameB=false
//   childFirst app[tag=app counter=11 bump=11 appLoader=true] appB.counter=11 appB.tag=app
//   lateC copyTag=copyC app=tag=app x=5 appLoader=true
//   after app[tag=app counter=11 bump=11 appLoader=true] appB.counter=11

import java.io.IOException;
import java.io.InputStream;
import java.io.UncheckedIOException;
import java.lang.reflect.Method;

public class L5W103CrossLoaderSameName {
    public static class B {
        public static int counter;
        public static String tag = "app";

        public static int bump() {
            return ++counter;
        }
    }

    public static class A {
        public static String run() {
            B.counter = 10;
            int r = B.bump();
            return "tag=" + B.tag + " counter=" + B.counter + " bump=" + r + " appLoader="
                    + (B.class.getClassLoader() == L5W103CrossLoaderSameName.class.getClassLoader());
        }

        public static Class<?> bClass() {
            return B.class;
        }
    }

    public static class C {
        public static int x;
        public static String tag = "app";
    }

    public static class A2 {
        public static String run() {
            C.x = 5;
            return "tag=" + C.tag + " x=" + C.x + " appLoader="
                    + (C.class.getClassLoader() == L5W103CrossLoaderSameName.class.getClassLoader());
        }
    }

    static final String OUTER = L5W103CrossLoaderSameName.class.getName();
    static final String B_NAME = OUTER + "$B";
    static final String A_NAME = OUTER + "$A";
    static final String C_NAME = OUTER + "$C";

    static byte[] bytes(String binaryName) {
        String res = binaryName.substring(binaryName.lastIndexOf('.') + 1) + ".class";
        try (InputStream in = L5W103CrossLoaderSameName.class.getResourceAsStream(res)) {
            return in.readAllBytes();
        } catch (IOException e) {
            throw new UncheckedIOException(e);
        }
    }

    /** Defines its own copy of one class on request; delegates every load to the application loader. */
    static final class Fresh extends ClassLoader {
        Fresh() {
            super(L5W103CrossLoaderSameName.class.getClassLoader());
        }

        Class<?> def(String name) {
            byte[] b = bytes(name);
            return defineClass(name, b, 0, b.length);
        }
    }

    /** Child-first for `A` and `B`: defines its own copies of both. */
    static final class ChildFirst extends ClassLoader {
        ChildFirst() {
            super(L5W103CrossLoaderSameName.class.getClassLoader());
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (name.equals(A_NAME) || name.equals(B_NAME)) {
                synchronized (getClassLoadingLock(name)) {
                    Class<?> c = findLoadedClass(name);
                    if (c == null) {
                        byte[] b = bytes(name);
                        c = defineClass(name, b, 0, b.length);
                    }
                    return c;
                }
            }
            return super.loadClass(name, resolve);
        }
    }

    public static void main(String[] args) throws Exception {
        System.out.println("before " + A.run());
        for (int r = 1; r <= 3; r++) {
            Class<?> copy = new Fresh().def(B_NAME);
            copy.getField("tag").set(null, "copy" + r);
            copy.getField("counter").setInt(null, 1000 * r);
            String app = A.run();
            String copyState = "tag=" + copy.getField("tag").get(null)
                    + " counter=" + copy.getField("counter").getInt(null);
            Object bumped = copy.getMethod("bump").invoke(null);
            System.out.println("copy" + r + " app[" + app + "] copy[" + copyState + "] copyBump=" + bumped);
        }

        ChildFirst cf = new ChildFirst();
        Class<?> aCopy = cf.loadClass(A_NAME);
        String copyRun = (String) aCopy.getMethod("run").invoke(null);
        Class<?> copyB = (Class<?>) aCopy.getMethod("bClass").invoke(null);
        System.out.println("childFirst " + copyRun + " sameB=" + (copyB == B.class));
        String app = A.run();
        System.out.println("childFirst app[" + app + "] appB.counter=" + B.class.getField("counter").getInt(null)
                + " appB.tag=" + B.class.getField("tag").get(null));

        Class<?> cCopy = new Fresh().def(C_NAME);
        cCopy.getField("tag").set(null, "copyC");
        Method run2 = A2.class.getMethod("run");
        System.out.println("lateC copyTag=" + cCopy.getField("tag").get(null) + " app=" + run2.invoke(null));

        System.out.println("after app[" + A.run() + "] appB.counter=" + B.class.getField("counter").getInt(null));
    }
}
