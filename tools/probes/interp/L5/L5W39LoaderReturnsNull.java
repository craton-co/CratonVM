// Interpreter round i1, wave 39, lane L5 -- a VM-initiated load whose
// `loadClass` RETURNS null, or returns a class of another name (JVMS §5.3.2:
// the loader must return the class it was asked for). HotSpot fails the
// resolution with `NoClassDefFoundError: <internal name>` and no cause, records
// it against the constant-pool entry (JVMS §5.4.3: the second execution fails
// the same way without asking the loader), and never falls back to another
// loader, although the name is on the application class path here.
//
// Fix for `docs/internal/fixed-bugs/interpreter-L5-a-vm-initiated-loadclass-exception-is-swallowed-into-a-global-fallback-FIXED-20261005.md`
// (the "returns null" remainder). Before wave 39 (and under `--compatible`,
// by design) the drive treated the answer as "not answered" and the global
// store defined `$Target` in the application loader: every row `ok`, asked
// once.
//
// Positive control: `CRATONVM_DBG=access` prints one
// `[ACCESS-DBG] LOADER-NULL PROPAGATE #n ...` line per row (2), and the exit
// line `[ACCESS-DBG] LOADER census: ...` counts them in
// `loader-nulls-propagated=2`.
//
// Run (no setup):
//   javac -d out L5W39LoaderReturnsNull.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W39LoaderReturnsNull
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   null first=java.lang.NoClassDefFoundError: L5W39LoaderReturnsNull$Target cause=null
//   null second=java.lang.NoClassDefFoundError: L5W39LoaderReturnsNull$Target cause=null
//   null asked=1
//   wrong first=java.lang.NoClassDefFoundError: L5W39LoaderReturnsNull$Target cause=null
//   wrong second=java.lang.NoClassDefFoundError: L5W39LoaderReturnsNull$Target cause=null
//   wrong asked=1

import java.io.IOException;
import java.io.InputStream;
import java.lang.reflect.InvocationTargetException;

public class L5W39LoaderReturnsNull {
    static final String P = "L5W39LoaderReturnsNull$";

    static final class L extends ClassLoader {
        final boolean wrongName;
        int asked;

        L(boolean wrongName) {
            super(L5W39LoaderReturnsNull.class.getClassLoader());
            this.wrongName = wrongName;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (name.equals(P + "Target")) {
                asked++;
                return wrongName ? Object.class : null;
            }
            if (!name.equals(P + "Ref")) {
                return super.loadClass(name, resolve);
            }
            Class<?> c = findLoadedClass(name);
            if (c != null) {
                return c;
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

    static void run(String label, boolean wrongName) throws Exception {
        L l = new L(wrongName);
        Class<?> ref = l.loadClass(P + "Ref");
        for (String turn : new String[] {"first", "second"}) {
            try {
                Object r = ref.getMethod("make").invoke(null);
                System.out.println(label + " " + turn + "=ok " + r);
            } catch (InvocationTargetException e) {
                Throwable t = e.getCause();
                System.out.println(label + " " + turn + "=" + t + " cause=" + t.getCause());
            }
        }
        System.out.println(label + " asked=" + l.asked);
    }

    public static void main(String[] args) throws Exception {
        run("null", false);
        run("wrong", true);
    }

    public static class Target {
    }

    public static class Ref {
        public static Object make() {
            return new Target().getClass().getClassLoader().getClass().getSimpleName();
        }
    }
}
