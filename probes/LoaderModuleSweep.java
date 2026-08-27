import java.lang.invoke.*;
import java.lang.module.ModuleDescriptor;
import java.util.*;
import java.util.concurrent.*;

/** ClassLoader, Module, MethodHandles/MethodHandle/MethodType, ForkJoinTask and
 *  ForkJoinPool --- the last named large families on the bridge-kind retirement
 *  surface, plus java/lang/System$1 (JavaLangAccess) reached INDIRECTLY through
 *  the Module and ClassLoader calls that route through it.
 *
 *  Determinism: ForkJoin work is joined before it is read and every task is a
 *  pure function of its input, so no result depends on scheduling or on how many
 *  workers the pool has. Nothing prints a thread name, a pool size, a timing, or
 *  a classloader identity hash. */
public class LoaderModuleSweep {
    static String esc(String s) {
        StringBuilder b = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.toString();
    }
    static void p(String tag, Object v) {
        System.out.println(esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }
    static void t(String tag, ThrowingRun r) {
        try { r.run(); p(tag, "no-throw"); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }
    // `throws Throwable`, not `Exception`: MethodHandle.invokeExact is declared
    // to throw Throwable, and a narrower functional interface will not accept it.
    interface ThrowingRun { void run() throws Throwable; }

    static void loaders() throws Exception {
        ClassLoader app = LoaderModuleSweep.class.getClassLoader();
        p("app loader non-null", app != null);
        p("app loader name", app.getName());
        p("system loader is app", ClassLoader.getSystemClassLoader() == app);
        ClassLoader plat = app.getParent();
        p("parent non-null", plat != null);
        p("parent name", plat == null ? "null" : plat.getName());
        p("platform parent is boot(null)", plat != null && plat.getParent() == null);
        p("String loader is boot(null)", String.class.getClassLoader());
        p("Object loader is boot(null)", Object.class.getClassLoader());
        p("int.class loader", int.class.getClassLoader());
        p("loadClass java.lang.String", app.loadClass("java.lang.String").getName());
        p("loadClass self", app.loadClass(LoaderModuleSweep.class.getName()).getSimpleName());
        p("Class.forName", Class.forName("java.util.ArrayList").getName());
        p("Class.forName no-init", Class.forName("java.util.ArrayList", false, app).getName());
        t("loadClass missing", () -> app.loadClass("no.such.Klass"));
        t("Class.forName missing", () -> Class.forName("no.such.Klass"));
        p("getResource absent", app.getResource("no/such/resource.txt"));
        p("getResourceAsStream absent", app.getResourceAsStream("no/such/resource.txt"));
        p("getSystemResource absent", ClassLoader.getSystemResource("no/such/x"));
        p("class getResource absent", LoaderModuleSweep.class.getResource("/no/such/x"));
        // a resource that MUST exist in every JDK image
        p("boot resource java/lang/String.class findable",
          Object.class.getResource("/java/lang/Object.class") != null
          || ClassLoader.getSystemResource("java/lang/Object.class") != null);
        p("getResources absent hasMoreElements",
          app.getResources("no/such/x").hasMoreElements());
        p("definedPackages is array", app.getDefinedPackages() != null);
        p("getPackage-ish getDefinedPackage(absent)", app.getDefinedPackage("no.such.pkg"));
        p("setDefaultAssertionStatus ok", setAssert(app));
    }
    static String setAssert(ClassLoader c) {
        try { c.setDefaultAssertionStatus(false); return "no-throw"; }
        catch (Throwable t) { return "THREW " + t.getClass().getName(); }
    }

    static void modules() {
        Module m = LoaderModuleSweep.class.getModule();
        p("own module isNamed", m.isNamed());
        p("own module getName", m.getName());
        // NOT `m.toString()`: an UNNAMED module renders as
        // "unnamed module @<identityHashCode>", so printing it puts a VM-chosen
        // identity in the diff. That is the fourth harness artefact in this
        // survey -- print the shape, not the identity.
        p("own module toString shape",
          m.toString().startsWith("unnamed module @"));
        Module base = String.class.getModule();
        p("java.base isNamed", base.isNamed());
        p("java.base getName", base.getName());
        p("java.base toString", base.toString());
        p("java.base canRead itself", base.canRead(base));
        p("unnamed canRead java.base", m.canRead(base));
        p("java.base canRead unnamed", base.canRead(m));
        p("java.base isExported java.lang", base.isExported("java.lang"));
        p("java.base isExported internal", base.isExported("jdk.internal.misc"));
        p("java.base isExported to unnamed", base.isExported("java.lang", m));
        p("java.base isOpen java.lang", base.isOpen("java.lang"));
        p("java.base isOpen bogus pkg", base.isOpen("no.such.pkg"));
        p("java.base packages contains java.lang", base.getPackages().contains("java.lang"));
        p("java.base packages size > 100", base.getPackages().size() > 100);
        ModuleDescriptor d = base.getDescriptor();
        p("descriptor non-null", d != null);
        p("descriptor name", d == null ? "null" : d.name());
        p("descriptor isAutomatic", d == null ? "null" : String.valueOf(d.isAutomatic()));
        p("descriptor isOpen", d == null ? "null" : String.valueOf(d.isOpen()));
        p("unnamed descriptor is null", m.getDescriptor());
        p("getLayer of java.base non-null", base.getLayer() != null);
        p("boot layer findModule java.base",
          ModuleLayer.boot().findModule("java.base").isPresent());
        p("boot layer findModule bogus",
          ModuleLayer.boot().findModule("no.such.module").isPresent());
        p("boot layer modules non-empty", !ModuleLayer.boot().modules().isEmpty());
        // addExports on an unnamed module is a documented no-op that returns this
        p("unnamed addExports returns this", m.addExports("x", base) == m);
    }

    static void methodHandles() throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        p("lookupClass", l.lookupClass().getSimpleName());
        MethodType mt = MethodType.methodType(int.class, int.class, int.class);
        p("MethodType toString", mt);
        p("MethodType returnType", mt.returnType().getName());
        p("MethodType parameterCount", mt.parameterCount());
        p("MethodType descriptorString", mt.descriptorString());
        p("MethodType changeReturnType", mt.changeReturnType(long.class));
        p("MethodType generic", mt.generic());
        p("MethodType erase", mt.erase());
        p("MethodType wrap", MethodType.methodType(int.class).wrap());
        p("MethodType fromDescriptor",
          MethodType.fromMethodDescriptorString("(I)Ljava/lang/String;", null));

        MethodHandle max = l.findStatic(Math.class, "max", mt);
        p("findStatic type", max.type());
        p("findStatic invoke", (int) max.invokeExact(3, 7));
        MethodHandle len = l.findVirtual(String.class, "length", MethodType.methodType(int.class));
        p("findVirtual invoke", (int) len.invokeExact("hello"));
        MethodHandle cat = l.findVirtual(String.class, "concat",
            MethodType.methodType(String.class, String.class));
        p("findVirtual concat", (String) cat.invokeExact("ab", "cd"));
        MethodHandle bound = cat.bindTo("pre-");
        p("bindTo", (String) bound.invokeExact("post"));
        MethodHandle asType = max.asType(MethodType.methodType(Object.class, Object.class, Object.class));
        p("asType invoke", asType.invoke(1, 2));
        p("constant", (String) MethodHandles.constant(String.class, "k").invokeExact());
        p("identity", (int) MethodHandles.identity(int.class).invokeExact(5));
        MethodHandle drop = MethodHandles.dropArguments(
            MethodHandles.constant(String.class, "d"), 0, int.class);
        p("dropArguments", (String) drop.invokeExact(1));
        MethodHandle ins = MethodHandles.insertArguments(max, 0, 10);
        p("insertArguments", (int) ins.invokeExact(4));
        p("arrayElementGetter", (int) MethodHandles.arrayElementGetter(int[].class)
            .invokeExact(new int[]{7, 8}, 1));
        MethodHandle fld = l.findGetter(Holder.class, "v", int.class);
        p("findGetter", (int) fld.invokeExact(new Holder()));
        t("findStatic missing", () -> l.findStatic(Math.class, "nope", mt));
        t("findVirtual wrong type", () ->
            l.findVirtual(String.class, "length", MethodType.methodType(long.class)));
        t("invokeExact wrong signature", () -> { Object o = max.invokeExact(1L, 2L); });
        t("findGetter on missing field", () -> l.findGetter(Holder.class, "nope", int.class));
    }
    public static class Holder { public int v = 42; }

    static class Sum extends RecursiveTask<Long> {
        final long lo, hi;
        Sum(long lo, long hi) { this.lo = lo; this.hi = hi; }
        protected Long compute() {
            if (hi - lo <= 8) { long s = 0; for (long i = lo; i < hi; i++) s += i; return s; }
            long mid = (lo + hi) >>> 1;
            Sum a = new Sum(lo, mid), b = new Sum(mid, hi);
            a.fork();
            return b.compute() + a.join();
        }
    }
    static class Boom extends RecursiveTask<Long> {
        protected Long compute() { throw new IllegalStateException("task-boom"); }
    }

    static void forkJoin() throws Exception {
        ForkJoinPool pool = new ForkJoinPool(2);
        try {
            p("fj invoke sum 0..100", pool.invoke(new Sum(0, 100)));
            Sum s = new Sum(0, 1000);
            p("fj submit get", pool.submit(s).get());
            p("fj task isDone", s.isDone());
            p("fj task isCompletedNormally", s.isCompletedNormally());
            p("fj task isCancelled", s.isCancelled());
            p("fj getRawResult", s.getRawResult());
            Boom b = new Boom();
            t("fj failing invoke", () -> pool.invoke(b));
            p("fj failed isCompletedAbnormally", b.isCompletedAbnormally());
            p("fj failed exception class",
              b.getException() == null ? "null" : b.getException().getClass().getName());
            Sum c = new Sum(0, 10);
            p("fj cancel before run", c.cancel(true));
            p("fj cancelled isCancelled", c.isCancelled());
            p("fj commonPool non-null", ForkJoinPool.commonPool() != null);
            p("fj commonPool parallelism >= 1", ForkJoinPool.commonPool().getParallelism() >= 1);
            p("fj pool parallelism", pool.getParallelism());
            p("fj isShutdown before", pool.isShutdown());
            p("fj inForkJoinPool from main", ForkJoinTask.inForkJoinPool());
        } finally {
            pool.shutdown();
            p("fj awaitTermination", pool.awaitTermination(10, TimeUnit.SECONDS));
            p("fj isTerminated", pool.isTerminated());
        }
    }

    public static void main(String[] a) throws Throwable {
        loaders();
        modules();
        methodHandles();
        forkJoin();
        System.out.println("DONE LoaderModuleSweep");
    }
}
