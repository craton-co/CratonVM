// Interpreter round i1, wave 38, lane L5 -- JVMS §5.3: the initiating loader's
// own throwable is the resolution's, through every class-resolution door, not
// only `new` (`L5W37LoaderThrowPropagates`). The loader throws for `$Target`,
// a name that IS on the application class path, so a VM that swallows the
// throw and asks the global store finds a class.
//
// Doors: `new`, `invokestatic` (the static-owner drive, `dispatch_static.rs`),
// `getstatic` (the field-owner resolver, `field_access.rs`), `ldc` of a class
// literal, `anewarray`. Each row is a fresh loader; `ise` throws
// IllegalStateException (propagated as is), `cnfe` throws
// ClassNotFoundException (NoClassDefFoundError with it as the cause). A second
// call of the same entry shows whether the failure was recorded: a
// `LinkageError` is (the loader is not asked again), anything else is not
// (asked again).
//
// Run (no setup):
//   javac -d out L5W38LoaderThrowDoors.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W38LoaderThrowDoors
//
// Positive control: `CRATONVM_DBG=access` prints one
// `[ACCESS-DBG] LOADER-THROW PROPAGATE #<n> ...` line per loader call that
// failed a resolution (15 here: each `ise` row asks twice, each `cnfe` row
// once) and, at exit,
// `[ACCESS-DBG] LOADER census: mode=jdk-only loader-throws-propagated=15 ...`.
//
// Before wave 38 (from the code): every row `ok` (the global fallback loaded
// `$Target` into the application loader), `asks=1`. `--compatible` keeps that
// by design (AGENTS.md).
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   new ise=java.lang.IllegalStateException cause=null / java.lang.IllegalStateException asks=2
//   new cnfe=java.lang.NoClassDefFoundError cause=java.lang.ClassNotFoundException / java.lang.NoClassDefFoundError asks=1
//   invokestatic ise=java.lang.IllegalStateException cause=null / java.lang.IllegalStateException asks=2
//   invokestatic cnfe=java.lang.NoClassDefFoundError cause=java.lang.ClassNotFoundException / java.lang.NoClassDefFoundError asks=1
//   getstatic ise=java.lang.IllegalStateException cause=null / java.lang.IllegalStateException asks=2
//   getstatic cnfe=java.lang.NoClassDefFoundError cause=java.lang.ClassNotFoundException / java.lang.NoClassDefFoundError asks=1
//   ldc ise=java.lang.IllegalStateException cause=null / java.lang.IllegalStateException asks=2
//   ldc cnfe=java.lang.NoClassDefFoundError cause=java.lang.ClassNotFoundException / java.lang.NoClassDefFoundError asks=1
//   anewarray ise=java.lang.IllegalStateException cause=null / java.lang.IllegalStateException asks=2
//   anewarray cnfe=java.lang.NoClassDefFoundError cause=java.lang.ClassNotFoundException / java.lang.NoClassDefFoundError asks=1

import java.io.IOException;
import java.io.InputStream;

public class L5W38LoaderThrowDoors {
    static final String P = "L5W38LoaderThrowDoors$";

    static final class Throwing extends ClassLoader {
        final boolean cnfe;
        int asks;

        Throwing(boolean cnfe) {
            super(L5W38LoaderThrowDoors.class.getClassLoader());
            this.cnfe = cnfe;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                if (name.equals(P + "Target")) {
                    asks++;
                    if (cnfe) {
                        throw new ClassNotFoundException(name);
                    }
                    throw new IllegalStateException("refused " + name);
                }
                if (!name.equals(P + "Ref")) {
                    return super.loadClass(name, resolve);
                }
                try (InputStream in =
                        ClassLoader.getSystemResourceAsStream(name.replace('.', '/') + ".class")) {
                    byte[] b = in.readAllBytes();
                    return defineClass(name, b, 0, b.length);
                } catch (IOException e) {
                    throw new ClassNotFoundException(name, e);
                }
            }
        }
    }

    static String call(Class<?> ref, String method) {
        try {
            Object r = ref.getMethod(method).invoke(null);
            return "ok";
        } catch (java.lang.reflect.InvocationTargetException e) {
            Throwable t = e.getCause();
            return t.getClass().getName() + " cause="
                    + (t.getCause() == null ? "null" : t.getCause().getClass().getName());
        } catch (Throwable t) {
            return "probe " + t;
        }
    }

    static String again(Class<?> ref, String method) {
        String s = call(ref, method);
        int sp = s.indexOf(' ');
        return sp < 0 ? s : s.substring(0, sp);
    }

    public static void main(String[] args) throws Exception {
        for (String door : new String[] {"new", "invokestatic", "getstatic", "ldc", "anewarray"}) {
            for (boolean cnfe : new boolean[] {false, true}) {
                Throwing loader = new Throwing(cnfe);
                Class<?> ref = loader.loadClass(P + "Ref");
                String method = door.equals("new") ? "make" : door;
                String first = call(ref, method);
                String second = again(ref, method);
                System.out.println(door + " " + (cnfe ? "cnfe" : "ise") + "=" + first + " / "
                        + second + " asks=" + loader.asks);
            }
        }
    }

    public static class Target {
        public static int value = 5;

        public static int m() {
            return 7;
        }
    }

    public static class Ref {
        public static Object make() {
            return new Target();
        }

        public static Object invokestatic() {
            return Target.m();
        }

        public static Object getstatic() {
            return Target.value;
        }

        public static Object ldc() {
            return Target.class;
        }

        public static Object anewarray() {
            return new Target[1];
        }
    }
}
