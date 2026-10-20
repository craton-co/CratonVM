// Round 15 wave 6, lane sweepA
// (docs/known-issues/jit/r15w5-fieldowner2-review-findings-20260930.md, L2):
// a compile door's field peek must not fill a per-site resolution that JVMS
// 5.3.4 has not settled yet. The interpreter trusts a filled entry (its owner
// covers the cached declaring class) and never asks the loader-constraint
// question again, so the constraint HotSpot records at the site's first
// resolution was never recorded, and a later conflicting define was admitted.
//
// Loader B (parent: app) defines Base and its own Shared. Loader A (parent: B,
// child-first for Sub, Reader and Shared) defines Sub extends Base. Base
// declares `public static Shared f`. Reader (A) names `Sub.f` from two methods
// through ONE constant-pool Fieldref: applyAsInt (hot, compiled; the site sits
// in a branch never taken) and second() (called once, interpreted). Neither
// loader has loaded Shared when second() resolves the field, so HotSpot
// records the constraint "A and B agree on Shared" (LinkResolver::resolve_field
// -> check_field_loader_constraints). B then defines its Shared, and A's own
// define of Shared violates the constraint: LinkageError.
//
// Before the fix, CratonVM's compile of applyAsInt filled the entry, second()
// took it without settling, nothing was recorded, and A defined a second
// Shared ("defined, same as B's: false").
//
// Expected HotSpot 25 output (default and -Xint):
//   sub from A: true base from B: true
//   warm checksum=1380000
//   second read=1
//   Shared from B: true
//   Shared through A: java.lang.LinkageError
//   done
//
// CratonVM arms: default (--jdk-only), --nojit (the interpreter alone always
// settled), CRATONVM_FIELD_CONSTRAINT_BEFORE_FILL=0 (restores the old peek:
// the divergence above), CRATONVM_JIT_THRESHOLD=1.
//
// --compatible (the i50-L5 --compatible gates page, its last code-read gate):
// the compile door's peek refused and withheld its fill under `is_jdk_only`
// only, so under --compatible the compiled site could bake a field JVMS 5.3.4
// refuses although the `loader-constraints` row enforces the interpreter's
// half. It now asks the row (`ClassRealm::loader_constraints_on`): HotSpot's
// lines under --compatible (release build, Windows, Temurin 25.0.3);
// `CRATONVM_COMPAT_ALIGN=-loader-constraints` prints
// "Shared through A: defined, same as B's: false".
// (Copied from the round-15 probe R15SweepaConstraintPending.)
import java.io.IOException;
import java.io.InputStream;
import java.util.Set;
import java.util.function.IntUnaryOperator;

public class L5W63CompatFieldPeekConstraint {
    static final String OUTER = L5W63CompatFieldPeekConstraint.class.getName();
    static final String SHARED = OUTER + "$Shared";
    static final String BASE = OUTER + "$Base";
    static final String SUB = OUTER + "$Sub";
    static final String READER = OUTER + "$Reader";

    public static class Shared {
    }

    public static class Base {
        public static Shared f;
    }

    public static class Sub extends Base {
    }

    public static class Reader implements IntUnaryOperator {
        public Reader() {
        }

        /// Hot. The `Sub.f` site is never executed; it is only compiled.
        @Override
        public int applyAsInt(int mode) {
            if (mode < 0) {
                Object o = Sub.f;
                return o == null ? -2 : -3;
            }
            return mode * 3 + 1;
        }

        /// Called once, interpreted: the site's first resolution.
        public int second() {
            Object o = Sub.f;
            return o == null ? 1 : 2;
        }
    }

    /// Defines the names in `own` itself from this probe's class files, and
    /// delegates everything else to its parent.
    static final class ChildFirst extends ClassLoader {
        private final Set<String> own;

        ChildFirst(String name, ClassLoader parent, Set<String> own) {
            super(name, parent);
            this.own = own;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    c = own.contains(name) ? findClass(name) : getParent().loadClass(name);
                }
                if (resolve) {
                    resolveClass(c);
                }
                return c;
            }
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            String res = name.replace('.', '/') + ".class";
            try (InputStream in = L5W63CompatFieldPeekConstraint.class.getClassLoader().getResourceAsStream(res)) {
                if (in == null) {
                    throw new ClassNotFoundException(name);
                }
                byte[] b = in.readAllBytes();
                return defineClass(name, b, 0, b.length);
            } catch (IOException e) {
                throw new ClassNotFoundException(name, e);
            }
        }
    }

    public static void main(String[] args) throws Exception {
        ClassLoader app = L5W63CompatFieldPeekConstraint.class.getClassLoader();
        ChildFirst b = new ChildFirst("B", app, Set.of(BASE, SHARED));
        ChildFirst a = new ChildFirst("A", b, Set.of(SUB, READER, SHARED));
        // Sub (and through it Base) is loaded before the hot method compiles,
        // so the compile door's peek can answer the site from A's record.
        Class<?> sub = Class.forName(SUB, false, a);
        System.out.println("sub from A: " + (sub.getClassLoader() == a)
                + " base from B: " + (sub.getSuperclass().getClassLoader() == b));
        IntUnaryOperator reader = (IntUnaryOperator) a.loadClass(READER).getConstructor().newInstance();
        long sum = 0;
        for (int round = 0; round < 3; round++) {
            for (int k = 0; k < 40_000; k++) {
                sum += reader.applyAsInt(k & 7);
            }
            // Let a background compile of applyAsInt finish.
            Thread.sleep(100);
        }
        System.out.println("warm checksum=" + sum);
        Object second = reader.getClass().getMethod("second").invoke(reader);
        System.out.println("second read=" + second);
        Class<?> sharedB = Class.forName(SHARED, false, b);
        System.out.println("Shared from B: " + (sharedB.getClassLoader() == b));
        String verdict;
        try {
            Class<?> sharedA = a.loadClass(SHARED);
            verdict = "defined, same as B's: " + (sharedA == sharedB);
        } catch (LinkageError e) {
            verdict = e.getClass().getName();
        }
        System.out.println("Shared through A: " + verdict);
        System.out.println("done");
    }
}
