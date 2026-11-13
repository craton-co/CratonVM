// j27 wave 6 (lane T1DISP20): DISP20-6 of
// docs/feature-designs/native-dispatch-architecture.md -- "the
// bound invokeExact path after a child-first loader still costs
// microseconds". The original probe (R20DispMhLinkRows, C:\craton\jr20-ssh)
// is not in the repository; this is a self-contained reconstruction of its
// decisive rows:
//
// * a child-first loader defines its OWN copies of `Val` and `Chain` (same
//   binary names as the app loader's), so the names are ambiguous VM-wide --
//   the situation `CRATONVM_JIT_MH_CONSTANT_BIND_AMBIGUOUS_NAMES` was built for;
// * `same-own-site`: a constant same-loader handle at a monomorphic site of
//   its own (the row DISP20-6 measured at 2.1 us vs HotSpot 28 ns);
// * `same-exact-top`: the same handle at a site that also sees the
//   cross-loader handle (the control of the throwing rows);
// * `cross-*`: the child loader's handle at a site typed with the app
//   loader's `Val` -- HotSpot 25 throws IncompatibleClassChangeError for invokeExact (null or
//   not), invoke throws ClassCastException (the asType cast), and an
//   `Object`-typed invoke with the child's own `Val` succeeds.
//
// Run from a DIRECTORY class path (the child loader reads the nested classes'
// bytes as resources):
//   java -cp <dir> J27T1Disp20MhLinkRows                           > hs.txt
//   cratonvm --real-jdk   -cp <dir> J27T1Disp20MhLinkRows | diff - hs.txt
//   cratonvm --compatible -cp <dir> J27T1Disp20MhLinkRows | diff - hs.txt
//   (and once with --nojit)
// Timing (separate `time` lines, ns/op, median of 5 reps): -Dj27.time=true.
// Per-call door census: CRATONVM_DBG_JIT_MH_BIND=1 (`[mh-bind] ... exact`
// for the own site, `-> door` for the cross handle).
//
// Expected HotSpot 25 output (facts only; JVMS 5.4.3.5 / MethodHandle
// invokeExact type identity, asType reference cast):
//   fact loaders distinct=true same-name=true
//   fact same-own-site sum=8400000
//   fact same-exact-top sum=8400000
//   fact cross-exact throws=20000 first=java.lang.IncompatibleClassChangeError
//   fact cross-exact-null throws=20000 first=java.lang.IncompatibleClassChangeError
//   fact cross-invoke throws=20000 first=java.lang.ClassCastException
//   fact cross-invoke-child sum=840000
import java.io.IOException;
import java.io.InputStream;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.Arrays;

public class J27T1Disp20MhLinkRows {
    public static class Val {
        public final int v;

        public Val(int v) {
            this.v = v;
        }
    }

    public static class Chain {
        public static int top(Val x) {
            return x.v + 1;
        }
    }

    /** Defines `Val` and `Chain` itself, from the parent's class-file bytes. */
    static final class ChildFirst extends ClassLoader {
        ChildFirst(ClassLoader parent) {
            super(parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (!name.equals(Val.class.getName()) && !name.equals(Chain.class.getName())) {
                return super.loadClass(name, resolve);
            }
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                String res = name.replace('.', '/') + ".class";
                try (InputStream in = getParent().getResourceAsStream(res)) {
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
    }

    static final int N = 200_000;
    static final int M = 20_000;
    static final MethodHandle TOP;
    static final MethodHandle CROSS;
    static final Class<?> CHILD_VAL;

    static {
        MethodHandle top;
        MethodHandle cross;
        Class<?> childVal;
        try {
            MethodType t = MethodType.methodType(int.class, Val.class);
            top = MethodHandles.lookup().findStatic(Chain.class, "top", t);
            ClassLoader child = new ChildFirst(J27T1Disp20MhLinkRows.class.getClassLoader());
            Class<?> childChain = Class.forName(Chain.class.getName(), true, child);
            childVal = Class.forName(Val.class.getName(), true, child);
            cross = MethodHandles.publicLookup()
                    .findStatic(childChain, "top", MethodType.methodType(int.class, childVal));
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
        TOP = top;
        CROSS = cross;
        CHILD_VAL = childVal;
    }

    static volatile Object sink;

    /** The own site: only TOP ever reaches it. */
    static long ownSite(Val v, int n) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += (int) TOP.invokeExact(v);
        }
        return s;
    }

    /** The shared site: TOP and CROSS both reach it. */
    static int exactTop(MethodHandle h, Val v) throws Throwable {
        return (int) h.invokeExact(v);
    }

    static int invokeTop(MethodHandle h, Val v) throws Throwable {
        return (int) h.invoke(v);
    }

    static int invokeObject(MethodHandle h, Object v) throws Throwable {
        return (int) h.invoke(v);
    }

    static long exactTopLoop(MethodHandle h, Val v, int n) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += exactTop(h, v);
        }
        return s;
    }

    interface Call {
        int call() throws Throwable;
    }

    static String throwsRow(Call c, int n) {
        int throwsCount = 0;
        String first = "none";
        for (int i = 0; i < n; i++) {
            try {
                sink = c.call();
            } catch (Throwable t) {
                if (throwsCount++ == 0) {
                    first = t.getClass().getName();
                }
            }
        }
        return "throws=" + throwsCount + " first=" + first;
    }

    public static void main(String[] args) throws Throwable {
        Val v = new Val(41);
        Object childV = CHILD_VAL.getConstructor(int.class).newInstance(41);
        System.out.println("fact loaders distinct=" + (CHILD_VAL != Val.class)
                + " same-name=" + CHILD_VAL.getName().equals(Val.class.getName()));
        System.out.println("fact same-own-site sum=" + ownSite(v, N));
        System.out.println("fact same-exact-top sum=" + exactTopLoop(TOP, v, N));
        System.out.println("fact cross-exact " + throwsRow(() -> exactTop(CROSS, v), M));
        System.out.println("fact cross-exact-null " + throwsRow(() -> exactTop(CROSS, null), M));
        System.out.println("fact cross-invoke " + throwsRow(() -> invokeTop(CROSS, v), M));
        long s = 0;
        for (int i = 0; i < M; i++) {
            s += invokeObject(CROSS, childV);
        }
        System.out.println("fact cross-invoke-child sum=" + s);
        if (Boolean.getBoolean("j27.time")) {
            time("same-own-site", n -> ownSite(v, n), 1_000_000);
            time("same-exact-top", n -> exactTopLoop(TOP, v, n), 1_000_000);
            time("cross-exact-throw", n -> {
                sink = throwsRow(() -> exactTop(CROSS, v), n);
                return n;
            }, 20_000);
        }
    }

    interface Row {
        long run(int n) throws Throwable;
    }

    static void time(String name, Row row, int n) throws Throwable {
        row.run(n); // warm
        long[] reps = new long[5];
        for (int r = 0; r < reps.length; r++) {
            long t0 = System.nanoTime();
            sink = row.run(n);
            reps[r] = System.nanoTime() - t0;
        }
        Arrays.sort(reps);
        System.out.println("time " + name + " " + (reps[2] / (double) n) + " ns/op");
    }
}
