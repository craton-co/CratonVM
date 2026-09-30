// Interpreter round i1, wave 41, lane L5 -- one VM-initiated resolution asks
// the initiating loader ONCE (JVMS §5.3.2), and its throwable is the
// resolution's, for a name in one of CratonVM's enterprise stub prefixes
// (`org/jboss/`, `io/quarkus/`, ...) that exists nowhere.
//
// `$Ref` is defined by a loader that overrides `loadClass` (and throws
// `IllegalStateException`, or `ClassNotFoundException`, for the missing
// name); its class file references `$Gone` renamed, in place, to
// `org/jboss/l5w41/Missing1` (same length), so its `new` asks the loader for
// `org.jboss.l5w41.Missing1`.
//
// Setup: CratonVM must run with `CRATONVM_LOADER_AWARE_RESOLUTION=0`. With
// that lever off, `resolve_class_loader_aware` (`constants.rs`) takes its
// global-first arm, whose "stub pre-pass" asked the loader through the
// unchecked drive, dropped its throw, let the global route miss (`--jdk-only`
// never fabricates the stub), and then asked the loader a SECOND time through
// the checked door: `asked=2` on every row (i37-L5's "stub pre-pass"
// remainder). With the lever on (the default) the loader-first arm answers
// and the probe matches HotSpot on `dev` too. `--compatible`: unchanged by
// design (the pre-pass keeps its unchecked ask; the rows print whatever the
// stub fallback gives there).
//
// Run:
//   javac -d out L5W41StubPrepassAsk.java
//   CRATONVM_LOADER_AWARE_RESOLUTION=0 cratonvm --java-home <jdk25> [--nojit] -cp out L5W41StubPrepassAsk
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   ise=java.lang.IllegalStateException: refused org.jboss.l5w41.Missing1 cause=null
//   ise asked=1
//   cnfe=java.lang.NoClassDefFoundError: org/jboss/l5w41/Missing1 cause=java.lang.ClassNotFoundException: refused org.jboss.l5w41.Missing1
//   cnfe asked=1

import java.io.IOException;
import java.io.InputStream;
import java.lang.reflect.InvocationTargetException;

public class L5W41StubPrepassAsk {
    static final String P = "L5W41StubPrepassAsk$";
    static final String MISSING = "org.jboss.l5w41.Missing1";

    /** `$Ref`'s class file with every `L5W41StubPrepassAsk$Gone` renamed. */
    static byte[] refBytes() throws IOException {
        byte[] b;
        try (InputStream in = ClassLoader.getSystemResourceAsStream(P + "Ref.class")) {
            b = in.readAllBytes();
        }
        byte[] from = (P + "Gone").getBytes();
        byte[] to = MISSING.replace('.', '/').getBytes();
        if (from.length != to.length) {
            throw new AssertionError(from.length + " != " + to.length);
        }
        for (int i = 0; i + from.length <= b.length; i++) {
            boolean hit = true;
            for (int j = 0; j < from.length && hit; j++) {
                hit = b[i + j] == from[j];
            }
            if (hit) {
                System.arraycopy(to, 0, b, i, to.length);
            }
        }
        return b;
    }

    static final class L extends ClassLoader {
        final boolean cnfe;
        int asked;

        L(boolean cnfe) {
            super(L5W41StubPrepassAsk.class.getClassLoader());
            this.cnfe = cnfe;
        }

        Class<?> defineRef() throws IOException {
            byte[] b = refBytes();
            return defineClass(P + "Ref", b, 0, b.length);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (name.equals(MISSING)) {
                asked++;
                if (cnfe) {
                    throw new ClassNotFoundException("refused " + name);
                }
                throw new IllegalStateException("refused " + name);
            }
            return super.loadClass(name, resolve);
        }
    }

    static String describe(Throwable t) {
        Throwable c = t.getCause();
        return t.getClass().getName() + ": " + t.getMessage() + " cause="
                + (c == null ? "null" : c.getClass().getName() + ": " + c.getMessage());
    }

    public static void main(String[] args) throws Exception {
        for (String row : new String[] {"ise", "cnfe"}) {
            L l = new L(row.equals("cnfe"));
            Class<?> ref = l.defineRef();
            try {
                ref.getMethod("make").invoke(null);
                System.out.println(row + "=ok");
            } catch (InvocationTargetException e) {
                System.out.println(row + "=" + describe(e.getCause()));
            }
            System.out.println(row + " asked=" + l.asked);
        }
    }

    public static class Gone {
    }

    public static class Ref {
        public static Object make() {
            return new Gone();
        }
    }
}
