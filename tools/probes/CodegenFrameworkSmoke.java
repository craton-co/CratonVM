// The frameworks a corpus run names, exercised as themselves.
//
// Written after the Groovy cluster (18 corpus classes, one null Set): a 35-row
// probe of the dynamic-class-generation MECHANISM -- Proxy, LambdaMetafactory,
// Lookup.defineClass, defineHiddenClass, ClassLoader.defineClass, ClassValue --
// came back clean while every one of those 18 classes was still dead. The null
// only became visible when REAL JDK BYTECODE read the field, and only a real
// framework bootstrap got there.
//
// So this probe deliberately does NOT stand in for the frameworks. Each row
// boots the actual jar and makes it generate and run a class.
//
// Every row prints a value or an exception CLASS NAME -- never a message, and
// never a generated class's own name, both of which carry addresses/counters
// that differ run to run and would drown the signal in false diffs.
//
// Reflective throughout so the probe compiles with none of the jars on javac's
// classpath; the `present` guard per family is what decides, and a MISSING jar
// prints as MISSING rather than passing silently.
import java.lang.reflect.Method;

public class CodegenFrameworkSmoke {
    interface Body { Object run() throws Throwable; }

    static void t(String tag, Body b) {
        String v;
        try { v = String.valueOf(b.run()); }
        catch (Throwable e) {
            Throwable root = e;
            while (root.getCause() != null && root.getCause() != root) root = root.getCause();
            v = "throws " + e.getClass().getName()
              + (root == e ? "" : " <- " + root.getClass().getName());
        }
        System.out.println(tag + " = " + v);
    }

    static boolean has(String cls) {
        try { Class.forName(cls, false, CodegenFrameworkSmoke.class.getClassLoader()); return true; }
        catch (Throwable e) { return false; }
    }

    public static void main(String[] a) {
        asm();
        byteBuddy();
        mockito();
        javassist();
        objenesis();
        groovy();
        System.out.println("DONE");
    }

    // ---- the target every framework generates against ----------------------

    public interface Greeter { String greet(String who); }

    public static class Base {
        public String hello() { return "base"; }
        public int twice(int n) { return n * 2; }
    }

    static class Defining extends ClassLoader {
        Defining() { super(CodegenFrameworkSmoke.class.getClassLoader()); }
        Class<?> def(String name, byte[] b) { return defineClass(name, b, 0, b.length); }
    }

    // ---- 1. ASM: emit an interface implementation by hand -------------------

    static void asm() {
        if (!has("org.objectweb.asm.ClassWriter")) { System.out.println("asm.present = MISSING"); return; }
        System.out.println("asm.present = true");
        t("asm.generateAndCall", () -> {
            Class<?> cwc = Class.forName("org.objectweb.asm.ClassWriter");
            Class<?> mvc = Class.forName("org.objectweb.asm.MethodVisitor");
            Object cw = cwc.getConstructor(int.class).newInstance(2); // COMPUTE_MAXS
            Method visit = cwc.getMethod("visit", int.class, int.class, String.class,
                String.class, String.class, String[].class);
            visit.invoke(cw, 61, 0x21, "AsmGreeter", null,
                "java/lang/Object", new String[] { "CodegenFrameworkSmoke$Greeter" });
            Method visitMethod = cwc.getMethod("visitMethod", int.class, String.class,
                String.class, String.class, String[].class);
            Object mv = visitMethod.invoke(cw, 1, "<init>", "()V", null, null);
            mvc.getMethod("visitCode").invoke(mv);
            mvc.getMethod("visitVarInsn", int.class, int.class).invoke(mv, 25, 0);
            mvc.getMethod("visitMethodInsn", int.class, String.class, String.class,
                String.class, boolean.class)
                .invoke(mv, 183, "java/lang/Object", "<init>", "()V", false);
            mvc.getMethod("visitInsn", int.class).invoke(mv, 177);
            mvc.getMethod("visitMaxs", int.class, int.class).invoke(mv, 1, 1);
            mvc.getMethod("visitEnd").invoke(mv);
            mv = visitMethod.invoke(cw, 1, "greet",
                "(Ljava/lang/String;)Ljava/lang/String;", null, null);
            mvc.getMethod("visitCode").invoke(mv);
            mvc.getMethod("visitLdcInsn", Object.class).invoke(mv, "asm-hi");
            mvc.getMethod("visitInsn", int.class).invoke(mv, 176);
            mvc.getMethod("visitMaxs", int.class, int.class).invoke(mv, 1, 2);
            mvc.getMethod("visitEnd").invoke(mv);
            cwc.getMethod("visitEnd").invoke(cw);
            byte[] bytes = (byte[]) cwc.getMethod("toByteArray").invoke(cw);
            Class<?> c = new Defining().def("AsmGreeter", bytes);
            Greeter g = (Greeter) c.getDeclaredConstructor().newInstance();
            return g.greet("x");
        });
    }

    // ---- 2. ByteBuddy: subclass + intercept --------------------------------

    static void byteBuddy() {
        if (!has("net.bytebuddy.ByteBuddy")) { System.out.println("bb.present = MISSING"); return; }
        System.out.println("bb.present = true");
        t("bb.subclassAndCall", () -> {
            Class<?> bb = Class.forName("net.bytebuddy.ByteBuddy");
            Object b = bb.getConstructor().newInstance();
            Object builder = bb.getMethod("subclass", Class.class).invoke(b, Base.class);
            Class<?> dyn = Class.forName("net.bytebuddy.dynamic.DynamicType$Builder");
            Class<?> elMatchers = Class.forName("net.bytebuddy.matcher.ElementMatchers");
            Object matcher = elMatchers.getMethod("named", String.class).invoke(null, "hello");
            Class<?> fixedValue = Class.forName("net.bytebuddy.implementation.FixedValue");
            Object impl = fixedValue.getMethod("value", Object.class).invoke(null, "bb-hi");
            Object md = dyn.getMethod("method",
                Class.forName("net.bytebuddy.matcher.ElementMatcher")).invoke(builder, matcher);
            Object intercepted = Class
                .forName("net.bytebuddy.dynamic.DynamicType$Builder$MethodDefinition$ImplementationDefinition")
                .getMethod("intercept", Class.forName("net.bytebuddy.implementation.Implementation"))
                .invoke(md, impl);
            Object made = dyn.getMethod("make").invoke(intercepted);
            Class<?> unloaded = Class.forName("net.bytebuddy.dynamic.DynamicType$Unloaded");
            Class<?> strategy = Class.forName("net.bytebuddy.dynamic.loading.ClassLoadingStrategy$Default");
            Object wrapper = strategy.getField("WRAPPER").get(null);
            Object loaded = unloaded.getMethod("load", ClassLoader.class,
                Class.forName("net.bytebuddy.dynamic.loading.ClassLoadingStrategy"))
                .invoke(made, CodegenFrameworkSmoke.class.getClassLoader(), wrapper);
            Class<?> c = (Class<?>) Class.forName("net.bytebuddy.dynamic.DynamicType$Loaded")
                .getMethod("getLoaded").invoke(loaded);
            Base o = (Base) c.getDeclaredConstructor().newInstance();
            return o.hello() + "/" + o.twice(21);
        });
    }

    // ---- 3. Mockito --------------------------------------------------------

    static void mockito() {
        if (!has("org.mockito.Mockito")) { System.out.println("mk.present = MISSING"); return; }
        System.out.println("mk.present = true");
        t("mk.mockInterface", () -> {
            Class<?> m = Class.forName("org.mockito.Mockito");
            Object mock = m.getMethod("mock", Class.class).invoke(null, Greeter.class);
            Object ongoing = m.getMethod("when", Object.class)
                .invoke(null, ((Greeter) mock).greet("a"));
            Class<?> stubber = Class.forName("org.mockito.stubbing.OngoingStubbing");
            stubber.getMethod("thenReturn", Object.class).invoke(ongoing, "mk-hi");
            return ((Greeter) mock).greet("a");
        });
        t("mk.mockClass", () -> {
            Class<?> m = Class.forName("org.mockito.Mockito");
            Object mock = m.getMethod("mock", Class.class).invoke(null, Base.class);
            // An unstubbed int-returning mock answers 0. A VALUE, so a mock that
            // silently forwards to the real method reads as 10 and is visible.
            return ((Base) mock).twice(5);
        });
    }

    // ---- 4. Javassist ------------------------------------------------------

    static void javassist() {
        if (!has("javassist.ClassPool")) { System.out.println("ja.present = MISSING"); return; }
        System.out.println("ja.present = true");
        t("ja.makeAndCall", () -> {
            Class<?> poolC = Class.forName("javassist.ClassPool");
            Object pool = poolC.getMethod("getDefault").invoke(null);
            Class<?> ctC = Class.forName("javassist.CtClass");
            Object ct = poolC.getMethod("makeClass", String.class).invoke(pool, "JaGreeter");
            Object iface = poolC.getMethod("get", String.class)
                .invoke(pool, "CodegenFrameworkSmoke$Greeter");
            ctC.getMethod("addInterface", ctC).invoke(ct, iface);
            Class<?> ctNew = Class.forName("javassist.CtNewMethod");
            Object mth = ctNew.getMethod("make", String.class, ctC)
                .invoke(null, "public String greet(String who) { return \"ja-hi\"; }", ct);
            ctC.getMethod("addMethod", Class.forName("javassist.CtMethod")).invoke(ct, mth);
            Class<?> c = (Class<?>) ctC.getMethod("toClass", Class.class)
                .invoke(ct, CodegenFrameworkSmoke.class);
            Greeter g = (Greeter) c.getDeclaredConstructor().newInstance();
            return g.greet("x");
        });
    }

    // ---- 5. Objenesis: construct without a constructor ----------------------

    static void objenesis() {
        if (!has("org.objenesis.ObjenesisStd")) { System.out.println("ob.present = MISSING"); return; }
        System.out.println("ob.present = true");
        t("ob.newInstance", () -> {
            Class<?> o = Class.forName("org.objenesis.ObjenesisStd");
            Object oi = o.getConstructor().newInstance();
            Object made = Class.forName("org.objenesis.Objenesis")
                .getMethod("newInstance", Class.class).invoke(oi, Base.class);
            return made != null && made.getClass() == Base.class;
        });
    }

    // ---- 6. Groovy: the cluster this campaign already fixed -----------------

    static void groovy() {
        if (!has("groovy.lang.GroovyShell")) { System.out.println("gr.present = MISSING"); return; }
        System.out.println("gr.present = true");
        t("gr.evaluate", () -> {
            Class<?> shell = Class.forName("groovy.lang.GroovyShell");
            Object sh = shell.getConstructor().newInstance();
            return shell.getMethod("evaluate", String.class).invoke(sh, "1+1");
        });
        t("gr.closure", () -> {
            Class<?> shell = Class.forName("groovy.lang.GroovyShell");
            Object sh = shell.getConstructor().newInstance();
            return shell.getMethod("evaluate", String.class)
                .invoke(sh, "def f = { a, b -> a * b }; f(6, 7)");
        });
        t("gr.systemVersion", () -> {
            Class<?> gs = Class.forName("groovy.lang.GroovySystem");
            // The class the 18-class cluster reported. A boolean, not the
            // version itself: a version string is stable but the registry
            // behind it is not, and the row is about reachability.
            return gs.getMethod("getVersion").invoke(null) != null;
        });
    }
}
