// Dynamic bytecode generation under `--jdk-only`, with NO third-party jar.
//
// The 2 848-class corpus run reports 55 failures whose largest cluster is
// Groovy (18 classes), surfacing as
//
//   NoClassDefFoundError: groovy/lang/GroovySystem
//   ExceptionInInitializerError
//
// and the reporter's own observation is that "virtually everything that broke
// touches dynamic bytecode generation". That is a MECHANISM, and every part of
// it is reachable from the JDK alone: Groovy's runtime generates classes the
// same ways `java.lang.reflect.Proxy`, `LambdaMetafactory` and
// `Lookup.defineClass`/`defineHiddenClass` do, and its `GroovySystem.<clinit>`
// bootstrap is built on `ClassValue` (`GroovyClassValueJava7 extends
// ClassValue`), which this tree has already had two recorded defects in.
//
// So this probe is the cluster's mechanism without its jar. A row that differs
// here names a defect the corpus can only report as "Groovy failed".
//
// A NoClassDefFoundError is printed distinctly: under `--jdk-only` that is the
// fabrication-refusal-with-no-recovery shape the definition-of-done screens
// for, and it must never be confused with an ordinary refusal.
//
// Hygiene: stdout only. No identity hashes, no generated class NAMES (the JDK
// picks those and both VMs may pick differently) -- only shapes, counts and
// exception class names.
import java.lang.invoke.CallSite;
import java.lang.invoke.LambdaMetafactory;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;
import java.util.ArrayList;
import java.util.List;
import java.util.function.Function;
import java.util.function.Supplier;

public class DynClassGenSweep {
    interface Body { Object run() throws Throwable; }

    static void t(String tag, Body b) {
        String v;
        try {
            Object o = b.run();
            v = String.valueOf(o);
        } catch (Throwable e) {
            String k = e.getClass().getName();
            if (e instanceof NoClassDefFoundError) {
                v = "NCDFE " + e.getMessage();
            } else if (e instanceof ExceptionInInitializerError && e.getCause() != null) {
                v = "EIIE cause=" + e.getCause().getClass().getName();
            } else {
                v = "throws " + k;
            }
        }
        System.out.println(tag + " = " + v);
    }

    public interface Greeter { String greet(String who); }
    public static class Base { public int v = 1; public int get() { return v; } }

    public static void main(String[] a) {
        classValue();
        proxies();
        lambdas();
        lookupDefine();
        customLoader();
        System.out.println("DONE");
    }

    // ---- ClassValue: the spine of Groovy's ClassInfo / MetaClassRegistry ----

    static void classValue() {
        t("cv.computeOnce", () -> {
            int[] calls = new int[1];
            ClassValue<String> cv = new ClassValue<>() {
                protected String computeValue(Class<?> type) {
                    calls[0]++;
                    return type.getName();
                }
            };
            String a1 = cv.get(String.class);
            String a2 = cv.get(String.class);
            return calls[0] + "/" + a1.equals(a2);
        });
        t("cv.identityStable", () -> {
            ClassValue<Object> cv = new ClassValue<>() {
                protected Object computeValue(Class<?> type) { return new Object(); }
            };
            return cv.get(Integer.class) == cv.get(Integer.class);
        });
        t("cv.perClass", () -> {
            ClassValue<String> cv = new ClassValue<>() {
                protected String computeValue(Class<?> type) { return type.getSimpleName(); }
            };
            return cv.get(String.class) + "," + cv.get(Integer.class);
        });
        t("cv.remove", () -> {
            int[] calls = new int[1];
            ClassValue<Integer> cv = new ClassValue<>() {
                protected Integer computeValue(Class<?> type) { return ++calls[0]; }
            };
            cv.get(Long.class);
            cv.remove(Long.class);
            cv.get(Long.class);
            return calls[0];
        });
        // Groovy mutates the value it got back and expects the SAME instance
        // later -- ClassInfo/ExpandoMetaClass depend on that identity.
        t("cv.mutateThenReRead", () -> {
            ClassValue<List<String>> cv = new ClassValue<>() {
                protected List<String> computeValue(Class<?> type) { return new ArrayList<>(); }
            };
            cv.get(Double.class).add("x");
            return cv.get(Double.class).size();
        });
        t("cv.computeValueThrows", () -> {
            ClassValue<String> cv = new ClassValue<>() {
                protected String computeValue(Class<?> type) {
                    throw new IllegalStateException("boom");
                }
            };
            return cv.get(Byte.class);
        });
    }

    // ---- java.lang.reflect.Proxy: JDK-generated bytecode --------------------

    static void proxies() {
        t("proxy.invoke", () -> {
            InvocationHandler h = (p, m, args) -> "hi " + args[0];
            Greeter g = (Greeter) Proxy.newProxyInstance(
                DynClassGenSweep.class.getClassLoader(),
                new Class<?>[] { Greeter.class }, h);
            return g.greet("bob");
        });
        t("proxy.isProxyClass", () -> {
            Object p = Proxy.newProxyInstance(
                DynClassGenSweep.class.getClassLoader(),
                new Class<?>[] { Greeter.class }, (a, b, c) -> null);
            return Proxy.isProxyClass(p.getClass());
        });
        t("proxy.interfacesKept", () -> {
            Object p = Proxy.newProxyInstance(
                DynClassGenSweep.class.getClassLoader(),
                new Class<?>[] { Greeter.class, Runnable.class }, (a, b, c) -> null);
            return p.getClass().getInterfaces().length;
        });
        t("proxy.sameClassReused", () -> {
            ClassLoader cl = DynClassGenSweep.class.getClassLoader();
            Object p1 = Proxy.newProxyInstance(cl, new Class<?>[] { Greeter.class }, (a, b, c) -> null);
            Object p2 = Proxy.newProxyInstance(cl, new Class<?>[] { Greeter.class }, (a, b, c) -> null);
            return p1.getClass() == p2.getClass();
        });
        t("proxy.toStringDispatches", () -> {
            InvocationHandler h = (p, m, args) -> m.getName().equals("toString") ? "PROXY" : null;
            Object p = Proxy.newProxyInstance(
                DynClassGenSweep.class.getClassLoader(),
                new Class<?>[] { Greeter.class }, h);
            return p.toString();
        });
        t("proxy.handlerThrows", () -> {
            InvocationHandler h = (p, m, args) -> { throw new IllegalStateException("x"); };
            Greeter g = (Greeter) Proxy.newProxyInstance(
                DynClassGenSweep.class.getClassLoader(),
                new Class<?>[] { Greeter.class }, h);
            return g.greet("a");
        });
    }

    // ---- LambdaMetafactory: hidden classes, the indy road Groovy takes -----

    static void lambdas() {
        t("lambda.plain", () -> {
            Function<String, String> f = s -> s + "!";
            return f.apply("a");
        });
        t("lambda.capturing", () -> {
            String pre = "p-";
            Function<String, String> f = s -> pre + s;
            return f.apply("a");
        });
        t("lambda.methodRef", () -> {
            Function<String, Integer> f = String::length;
            return f.apply("abcd");
        });
        t("lambda.boundMethodRef", () -> {
            String s = "hello";
            Supplier<Integer> f = s::length;
            return f.get();
        });
        t("lambda.ctorRef", () -> {
            Supplier<ArrayList<String>> f = ArrayList::new;
            return f.get().size();
        });
        t("lambda.classIsHidden", () -> {
            Function<String, String> f = s -> s;
            // A lambda's implementation class is a HIDDEN class on JDK 15+.
            return f.getClass().isHidden();
        });
        // The metafactory called by hand, which is what a language runtime does.
        t("lambda.metafactoryByHand", () -> {
            MethodHandles.Lookup l = MethodHandles.lookup();
            MethodHandle impl = l.findStatic(DynClassGenSweep.class, "shout",
                MethodType.methodType(String.class, String.class));
            CallSite cs = LambdaMetafactory.metafactory(
                l, "apply",
                MethodType.methodType(Function.class),
                MethodType.methodType(Object.class, Object.class),
                impl,
                MethodType.methodType(String.class, String.class));
            @SuppressWarnings("unchecked")
            Function<String, String> f = (Function<String, String>) cs.getTarget().invoke();
            return f.apply("x");
        });
    }

    public static String shout(String s) { return s.toUpperCase() + "!"; }

    // ---- Lookup.defineClass / defineHiddenClass ---------------------------
    //
    // Real bytes, read from a class this probe already ships, so no assembler
    // and no third-party jar is involved.

    static byte[] classBytes(Class<?> c) throws Exception {
        String res = c.getName().replace('.', '/') + ".class";
        try (java.io.InputStream in = c.getClassLoader().getResourceAsStream(res)) {
            if (in == null) return null;
            return in.readAllBytes();
        }
    }

    static void lookupDefine() {
        t("define.bytesReadable", () -> {
            byte[] b = classBytes(Base.class);
            return b != null && b.length > 0;
        });
        t("define.hiddenClass", () -> {
            byte[] b = classBytes(Base.class);
            if (b == null) return "NO-BYTES";
            MethodHandles.Lookup l = MethodHandles.lookup()
                .defineHiddenClass(b, true);
            Class<?> c = l.lookupClass();
            return c.isHidden() + "/" + (c.getSuperclass() == Object.class);
        });
        t("define.hiddenInstantiate", () -> {
            byte[] b = classBytes(Base.class);
            if (b == null) return "NO-BYTES";
            Class<?> c = MethodHandles.lookup().defineHiddenClass(b, true).lookupClass();
            Object o = c.getDeclaredConstructor().newInstance();
            return c.getMethod("get").invoke(o);
        });
        t("define.hiddenBadMagic", () -> {
            MethodHandles.lookup().defineHiddenClass(new byte[] { 1, 2, 3, 4 }, true);
            return "no-throw";
        });
        t("define.hiddenNullBytes", () -> {
            MethodHandles.lookup().defineHiddenClass(null, true);
            return "no-throw";
        });
        // All FOUR doors that take class bytes, so a fix to one is not applied
        // to the others on faith. HotSpot: bad magic is ClassFormatError at
        // every door; a null array is NPE.
        t("define.lookupBadMagic", () -> {
            MethodHandles.lookup().defineClass(new byte[] { 1, 2, 3, 4 });
            return "no-throw";
        });
        t("define.lookupNullBytes", () -> {
            MethodHandles.lookup().defineClass(null);
            return "no-throw";
        });
        t("define.lookupEmptyBytes", () -> {
            MethodHandles.lookup().defineClass(new byte[0]);
            return "no-throw";
        });
        t("define.hiddenEmptyBytes", () -> {
            MethodHandles.lookup().defineHiddenClass(new byte[0], true);
            return "no-throw";
        });
        // Correct MAGIC, truncated body. Distinguishes "we only check the four
        // magic bytes" from "we report a parse failure the way the JDK does":
        // both are ClassFormatError on HotSpot, and a fix that only touches the
        // magic check leaves this one wrong.
        t("define.lookupTruncated", () -> {
            MethodHandles.lookup().defineClass(
                new byte[] { (byte) 0xCA, (byte) 0xFE, (byte) 0xBA, (byte) 0xBE, 0, 0 });
            return "no-throw";
        });
        t("define.hiddenTruncated", () -> {
            MethodHandles.lookup().defineHiddenClass(
                new byte[] { (byte) 0xCA, (byte) 0xFE, (byte) 0xBA, (byte) 0xBE, 0, 0 }, true);
            return "no-throw";
        });
        // A null VARARGS array, which is a different argument from null bytes.
        t("define.hiddenNullOptions", () -> {
            byte[] b = classBytes(Base.class);
            if (b == null) return "NO-BYTES";
            MethodHandles.lookup().defineHiddenClass(b, true,
                (MethodHandles.Lookup.ClassOption[]) null);
            return "no-throw";
        });
        // The THIRD door of this family, which the rows above never reach.
        t("define.wcdOk", () -> {
            byte[] b = classBytes(Base.class);
            if (b == null) return "NO-BYTES";
            MethodHandles.Lookup l = MethodHandles.lookup()
                .defineHiddenClassWithClassData(b, "cd", true);
            return l.lookupClass().isHidden();
        });
        t("define.wcdBadMagic", () -> {
            MethodHandles.lookup()
                .defineHiddenClassWithClassData(new byte[] { 1, 2, 3, 4 }, "cd", true);
            return "no-throw";
        });
        t("define.wcdNullBytes", () -> {
            MethodHandles.lookup().defineHiddenClassWithClassData(null, "cd", true);
            return "no-throw";
        });
        // The CONTROL for the rows above: one case where the JDK really does
        // raise IllegalArgumentException, so a fix that turns every refusal on
        // this surface into ClassFormatError is visibly wrong here.
        t("define.lookupWrongPackage", () -> {
            byte[] b;
            try (java.io.InputStream in = ClassLoader
                    .getSystemResourceAsStream("java/util/ArrayList.class")) {
                if (in == null) return "NO-BYTES";
                b = in.readAllBytes();
            }
            MethodHandles.lookup().defineClass(b);
            return "no-throw";
        });
    }

    // ---- a custom ClassLoader calling defineClass --------------------------

    static class Loader extends ClassLoader {
        Loader() { super(DynClassGenSweep.class.getClassLoader()); }
        Class<?> def(String name, byte[] b) { return defineClass(name, b, 0, b.length); }
    }

    static void customLoader() {
        t("loader.defineAndUse", () -> {
            byte[] b = classBytes(Base.class);
            if (b == null) return "NO-BYTES";
            Loader l = new Loader();
            Class<?> c = l.def(Base.class.getName(), b);
            Object o = c.getDeclaredConstructor().newInstance();
            return c.getMethod("get").invoke(o) + "/" + (c != Base.class);
        });
        t("loader.definedLoaderIsMine", () -> {
            byte[] b = classBytes(Base.class);
            if (b == null) return "NO-BYTES";
            Loader l = new Loader();
            Class<?> c = l.def(Base.class.getName(), b);
            return c.getClassLoader() == l;
        });
        t("loader.duplicateDefine", () -> {
            byte[] b = classBytes(Base.class);
            if (b == null) return "NO-BYTES";
            Loader l = new Loader();
            l.def(Base.class.getName(), b);
            l.def(Base.class.getName(), b);
            return "no-throw";
        });
        t("loader.badMagic", () -> {
            new Loader().def("X$Bad", new byte[] { 1, 2, 3, 4 });
            return "no-throw";
        });
        t("loader.nullBytes", () -> {
            new Loader().def("X$Null", null);
            return "no-throw";
        });
        t("loader.truncatedBytes", () -> {
            byte[] b = classBytes(Base.class);
            if (b == null) return "NO-BYTES";
            byte[] half = java.util.Arrays.copyOf(b, b.length / 2);
            new Loader().def(Base.class.getName() + "$Half", half);
            return "no-throw";
        });
    }
}
