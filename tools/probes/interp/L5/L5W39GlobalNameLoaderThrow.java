// Interpreter round i1, wave 39, lane L5 -- a user loader that overrides
// `loadClass` is asked by the VM for a JDK-global name (`javax.*`: the JDK has
// the class) and throws, returns `null`, or returns a class of another name.
// HotSpot asks the loader for every name and its outcome is the resolution's:
// a `ClassNotFoundException` becomes `NoClassDefFoundError` with it as the
// cause, another throwable propagates as it is, `null` or a wrong-named class
// is `NoClassDefFoundError` without a cause. A `LinkageError` is recorded
// against the entry (the second execution does not ask the loader); an
// `IllegalStateException` is not (asked twice).
//
// Fix for `docs/internal/fixed-bugs/interpreter-L5-a-vm-initiated-loadclass-exception-is-swallowed-into-a-global-fallback-FIXED-20261005.md`
// (the JDK-global-name remainder). Before wave 39 (and under `--compatible`,
// by design) `constants.rs` `drive_loader_for_global_name` took every refusal
// as "not answered" and the global route answered java.naming's class: every
// row `ok`; asked once each (the refusal memoised the built-in class).
//
// Positive control: `CRATONVM_DBG=access` prints
// `[ACCESS-DBG] LOADER-THROW PROPAGATE ...` for the `ise` (twice) and `cnfe`
// rows, `[ACCESS-DBG] LOADER-NULL PROPAGATE ...` for `null` and `wrong`, each
// for the `ldc` and the `static` rows, and the exit census
// `loader-throws-propagated=6 ... loader-nulls-propagated=4`.
//
// The `static` rows are an `invokestatic javax.naming.spi.NamingManager`
// owner (`dispatch_static.rs`, the owner's JDK-global-name ask).
//
// Run (no setup):
//   javac -d out L5W39GlobalNameLoaderThrow.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W39GlobalNameLoaderThrow
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   ok first=ok javax.naming.Context
//   ok second=ok javax.naming.Context
//   ok asked=1
//   ok static first=ok false
//   ok static second=ok false
//   ok static asked=1
//   ise first=java.lang.IllegalStateException: refused javax.naming.Context cause=null
//   ise second=java.lang.IllegalStateException: refused javax.naming.Context cause=null
//   ise asked=2
//   ise static first=java.lang.IllegalStateException: refused javax.naming.spi.NamingManager cause=null
//   ise static second=java.lang.IllegalStateException: refused javax.naming.spi.NamingManager cause=null
//   ise static asked=2
//   cnfe first=java.lang.NoClassDefFoundError: javax/naming/Context cause=java.lang.ClassNotFoundException: javax.naming.Context
//   cnfe second=java.lang.NoClassDefFoundError: javax/naming/Context cause=java.lang.ClassNotFoundException: javax.naming.Context
//   cnfe asked=1
//   cnfe static first=java.lang.NoClassDefFoundError: javax/naming/spi/NamingManager cause=java.lang.ClassNotFoundException: javax.naming.spi.NamingManager
//   cnfe static second=java.lang.NoClassDefFoundError: javax/naming/spi/NamingManager cause=java.lang.ClassNotFoundException: javax.naming.spi.NamingManager
//   cnfe static asked=1
//   null first=java.lang.NoClassDefFoundError: javax/naming/Context cause=null
//   null second=java.lang.NoClassDefFoundError: javax/naming/Context cause=null
//   null asked=1
//   null static first=java.lang.NoClassDefFoundError: javax/naming/spi/NamingManager cause=null
//   null static second=java.lang.NoClassDefFoundError: javax/naming/spi/NamingManager cause=null
//   null static asked=1
//   wrong first=java.lang.NoClassDefFoundError: javax/naming/Context cause=null
//   wrong second=java.lang.NoClassDefFoundError: javax/naming/Context cause=null
//   wrong asked=1
//   wrong static first=java.lang.NoClassDefFoundError: javax/naming/spi/NamingManager cause=null
//   wrong static second=java.lang.NoClassDefFoundError: javax/naming/spi/NamingManager cause=null
//   wrong static asked=1

import java.io.IOException;
import java.io.InputStream;
import java.lang.reflect.InvocationTargetException;

public class L5W39GlobalNameLoaderThrow {
    static final String P = "L5W39GlobalNameLoaderThrow$";
    static final String TARGET = "javax.naming.Context";
    static final String OWNER = "javax.naming.spi.NamingManager";

    static final class L extends ClassLoader {
        final String mode;
        int asked;
        int ownerAsked;

        L(String mode) {
            super(L5W39GlobalNameLoaderThrow.class.getClassLoader());
            this.mode = mode;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (name.equals(TARGET) || name.equals(OWNER)) {
                if (name.equals(TARGET)) {
                    asked++;
                } else {
                    ownerAsked++;
                }
                switch (mode) {
                    case "ise":
                        throw new IllegalStateException("refused " + name);
                    case "cnfe":
                        throw new ClassNotFoundException(name);
                    case "null":
                        return null;
                    case "wrong":
                        return Object.class;
                    default:
                        return super.loadClass(name, resolve);
                }
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

    static void run(String mode) throws Exception {
        L l = new L(mode);
        Class<?> ref = l.loadClass(P + "Ref");
        for (String turn : new String[] {"first", "second"}) {
            try {
                Object r = ref.getMethod("get").invoke(null);
                System.out.println(mode + " " + turn + "=ok " + r);
            } catch (InvocationTargetException e) {
                Throwable t = e.getCause();
                System.out.println(mode + " " + turn + "=" + t + " cause=" + t.getCause());
            }
        }
        System.out.println(mode + " asked=" + l.asked);
        for (String turn : new String[] {"first", "second"}) {
            try {
                Object r = ref.getMethod("call").invoke(null);
                System.out.println(mode + " static " + turn + "=ok " + r);
            } catch (InvocationTargetException e) {
                Throwable t = e.getCause();
                System.out.println(mode + " static " + turn + "=" + t + " cause=" + t.getCause());
            }
        }
        System.out.println(mode + " static asked=" + l.ownerAsked);
    }

    public static void main(String[] args) throws Exception {
        for (String mode : new String[] {"ok", "ise", "cnfe", "null", "wrong"}) {
            run(mode);
        }
    }

    public static class Ref {
        public static Object get() {
            return javax.naming.Context.class.getName();
        }

        public static Object call() {
            return javax.naming.spi.NamingManager.hasInitialContextFactoryBuilder();
        }
    }
}
