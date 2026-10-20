// Interpreter round i1, wave 6, lane L2 — natives and VM paths must hand Java
// a reference array of its DECLARED type (`T[]`), not an untyped `Object[]`.
//
// Compare stdout against HotSpot 25 (`java L2TypedNativeArrays`). Run CratonVM
// with `--compatible`, with and without `--nojit`: every row must print the
// same thing in both, and no row may throw `ClassCastException` (the array-cast
// rule is HotSpot's strict one in every mode since interpreter round i1 wave 10;
// the `CRATONVM_STRICT_ARRAY_CAST` lever this header used to name is retired).
//
// Expected HotSpot 25 output (the runtime class of each array, by JDK
// contract; the `cast` column is a `(T[])` cast of the value through Object):
//   t01 proxy Method.getParameterTypes  [Ljava.lang.Class; cast=ok len=2
//   t02 other-thread getStackTrace       [Ljava.lang.StackTraceElement; cast=ok
//   t03 Method.getParameters             [Ljava.lang.reflect.Parameter; cast=ok len=1
//   t04 MethodType.parameterArray        [Ljava.lang.Class; cast=ok len=2
//   t05 InetAddress.getAllByName         [Ljava.net.InetAddress; cast=ok
//   t06 String.getGenericInterfaces      [Ljava.lang.reflect.Type; cast=ok
//   t07 Object.getGenericInterfaces      [Ljava.lang.Class; len=0
//   t08 record getRecordComponents       [Ljava.lang.reflect.RecordComponent; cast=ok len=2
//   t09 getNestMembers                   [Ljava.lang.Class; cast=ok
//   t10 getDeclaredClasses               [Ljava.lang.Class; cast=ok
//   t11..t18 covariant array casts / instanceof (the fast verdict's new
//            shapes): true true false true true false true false
//   (wave 7, the remaining producers)
//   t20 Security.getProviders            [Ljava.security.Provider; cast=ok
//   t21 Security.getProviders(filter)    [Ljava.security.Provider; cast=ok
//   t22 getAllStackTraces value          [Ljava.lang.StackTraceElement; cast=ok
//   t23 Logger.getHandlers               [Ljava.util.logging.Handler; cast=ok len=0
//   t24 PropertyChangeSupport listeners  [Ljava.beans.PropertyChangeListener; cast=ok len=1
//   t25 FileVisitResult.values           [Ljava.nio.file.FileVisitResult; cast=ok len=4
//   t26 URLClassLoader.getURLs           [Ljava.net.URL; cast=ok len=1
//
// stderr only (timing, not compared): the covariant `checkcast` loop of t19.
import java.lang.invoke.MethodType;
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Proxy;
import java.net.InetAddress;

public class L2TypedNativeArrays {
    interface Svc {
        String call(int a, String b);
    }

    record Pt(int x, String y) {}

    static class Nested {}

    static String cls(Object o) {
        return o == null ? "null" : o.getClass().getName();
    }

    public static void main(String[] args) throws Exception {
        // t01: the VM-built proxy Method handed to InvocationHandler.invoke.
        final String[] seen = new String[1];
        InvocationHandler h = (proxy, m, a) -> {
            Class<?>[] p = m.getParameterTypes();
            Object o = p;
            String cast;
            try {
                Class<?>[] back = (Class<?>[]) o;
                cast = "ok len=" + back.length;
            } catch (ClassCastException e) {
                cast = "CCE";
            }
            seen[0] = cls(p) + " cast=" + cast;
            return "r";
        };
        Svc s = (Svc) Proxy.newProxyInstance(
                L2TypedNativeArrays.class.getClassLoader(), new Class<?>[] {Svc.class}, h);
        s.call(1, "x");
        System.out.println("t01 " + seen[0]);

        // t02: another live thread's stack (Thread.getStackTrace0).
        final Object lock = new Object();
        Thread t = new Thread(() -> {
            synchronized (lock) {
                try {
                    lock.wait();
                } catch (InterruptedException ignored) {
                }
            }
        });
        t.setDaemon(true);
        t.start();
        while (t.getState() != Thread.State.WAITING) {
            Thread.sleep(5);
        }
        StackTraceElement[] st = t.getStackTrace();
        Object sto = st;
        String stCast;
        try {
            StackTraceElement[] back = (StackTraceElement[]) sto;
            stCast = back != null ? "ok" : "null";
        } catch (ClassCastException e) {
            stCast = "CCE";
        }
        System.out.println("t02 " + cls(st) + " cast=" + stCast);
        synchronized (lock) {
            lock.notifyAll();
        }
        t.join();

        // t03: Executable.getParameters.
        Object params = Integer.class.getMethod("valueOf", int.class).getParameters();
        System.out.println("t03 " + cls(params) + " cast="
                + castTo(params, java.lang.reflect.Parameter[].class));

        // t04: MethodType.parameterArray.
        Object ptypes = MethodType.methodType(void.class, int.class, String.class).parameterArray();
        System.out.println("t04 " + cls(ptypes) + " cast=" + castTo(ptypes, Class[].class));

        // t05: InetAddress.getAllByName on a literal (no resolver involved).
        Object addrs = InetAddress.getAllByName("127.0.0.1");
        System.out.println("t05 " + cls(addrs) + " cast=" + castTo(addrs, InetAddress[].class)
                .replaceAll(" len=\\d+", ""));

        // t06 / t07: getGenericInterfaces with and without a Signature.
        Object gi = String.class.getGenericInterfaces();
        System.out.println("t06 " + cls(gi) + " cast="
                + castTo(gi, java.lang.reflect.Type[].class).replaceAll(" len=\\d+", ""));
        Object ogi = Object.class.getGenericInterfaces();
        System.out.println("t07 " + cls(ogi) + " len=" + ((Object[]) ogi).length);

        // t08: record components.
        Object rc = Pt.class.getRecordComponents();
        System.out.println("t08 " + cls(rc) + " cast="
                + castTo(rc, java.lang.reflect.RecordComponent[].class));

        // t09 / t10: nest members and declared classes.
        Object nm = L2TypedNativeArrays.class.getNestMembers();
        System.out.println("t09 " + cls(nm) + " cast="
                + castTo(nm, Class[].class).replaceAll(" len=\\d+", ""));
        Object dc = L2TypedNativeArrays.class.getDeclaredClasses();
        System.out.println("t10 " + cls(dc) + " cast="
                + castTo(dc, Class[].class).replaceAll(" len=\\d+", ""));

        // t11..t18: covariant array shapes answered by the fast verdict.
        Object strs = new String[] {"a"};
        Object ints = new Integer[] {1};
        Object nested = new String[][] {{"a"}};
        System.out.println("t11 " + (strs instanceof Object[]));
        System.out.println("t12 " + (strs instanceof Comparable[]));
        System.out.println("t13 " + (strs instanceof Number[]));
        System.out.println("t14 " + (ints instanceof Number[]));
        System.out.println("t15 " + (nested instanceof Cloneable[]));
        System.out.println("t16 " + (nested instanceof CharSequence[]));
        System.out.println("t17 " + castOk(nested, java.io.Serializable[].class));
        System.out.println("t18 " + castOk(strs, int[].class));

        // t20..t26 (wave 7): the producers the wave-6 survey left untyped.
        Object provs = java.security.Security.getProviders();
        System.out.println("t20 " + cls(provs) + " cast="
                + castTo(provs, java.security.Provider[].class).replaceAll(" len=\\d+", ""));
        Object mdProvs = java.security.Security.getProviders("MessageDigest.SHA-256");
        System.out.println("t21 " + cls(mdProvs) + " cast="
                + castTo(mdProvs, java.security.Provider[].class).replaceAll(" len=\\d+", ""));
        Object mine = Thread.getAllStackTraces().get(Thread.currentThread());
        System.out.println("t22 " + cls(mine) + " cast="
                + castTo(mine, StackTraceElement[].class).replaceAll(" len=\\d+", ""));
        Object handlers = java.util.logging.Logger.getLogger("l2.typed.arrays").getHandlers();
        System.out.println("t23 " + cls(handlers) + " cast="
                + castTo(handlers, java.util.logging.Handler[].class));
        java.beans.PropertyChangeSupport pcs = new java.beans.PropertyChangeSupport(lock);
        pcs.addPropertyChangeListener(e -> { });
        Object listeners = pcs.getPropertyChangeListeners();
        System.out.println("t24 " + cls(listeners) + " cast="
                + castTo(listeners, java.beans.PropertyChangeListener[].class));
        Object visits = java.nio.file.FileVisitResult.values();
        System.out.println("t25 " + cls(visits) + " cast="
                + castTo(visits, java.nio.file.FileVisitResult[].class));
        try (java.net.URLClassLoader ucl = new java.net.URLClassLoader(
                new java.net.URL[] {new java.io.File(".").toURI().toURL()})) {
            Object urls = ucl.getURLs();
            System.out.println("t26 " + cls(urls) + " cast="
                    + castTo(urls, java.net.URL[].class));
        }

        // t19: timing of a covariant checkcast site (stderr only).
        Object[] pool = {new String[1], new Integer[1], new StringBuilder[1]};
        long t0 = System.nanoTime();
        long n = 0;
        for (int i = 0; i < 3_000_000; i++) {
            Object[] a = (Object[]) pool[i % 3];
            n += a.length;
            if (pool[i % 3] instanceof Comparable[]) {
                n++;
            }
        }
        System.err.println("t19 covariant checkcast+instanceof loop: "
                + (System.nanoTime() - t0) / 1_000_000 + " ms (n=" + n + ")");
    }

    static String castTo(Object o, Class<?> arrayType) {
        try {
            Object back = arrayType.cast(o);
            return "ok len=" + ((Object[]) back).length;
        } catch (ClassCastException e) {
            return "CCE";
        }
    }

    static boolean castOk(Object o, Class<?> arrayType) {
        return arrayType.isInstance(o);
    }
}
