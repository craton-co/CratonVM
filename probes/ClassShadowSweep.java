import java.lang.reflect.*;
import java.util.*;

/** The 20 `java.lang.Class` triples the `--jdk-only-report` marks
 *  `outcome=native-won`, plus the 5 on `java.lang.Module`.
 *
 *  Third family off the Phase 2 worklist. The first two (`java.util.Arrays`,
 *  `java.util.HashMap`) yielded 14 defects and EVERY ONE was on a contract
 *  edge, so the same aim applies here: primitives, arrays, nested and anonymous
 *  classes, the public-vs-declared split, and the exact exception each lookup
 *  must throw.
 *
 *  `java.lang.Class` is the reflection surface every framework walks, and its
 *  naming methods are a nest of special cases that a from-memory implementation
 *  reliably gets wrong in the same places:
 *
 *    getName          int -> "int",  int[] -> "[I",  String[] -> "[Ljava.lang.String;"
 *    getSimpleName    int -> "int",  int[] -> "int[]",  anonymous -> ""
 *    getPackageName   defined for primitives and arrays, NOT ""
 *    descriptorString int -> "I",   String[] -> "[Ljava/lang/String;"
 *    getModifiers     a primitive is PUBLIC FINAL ABSTRACT
 *    forName          "int" is NOT findable; "[I" is
 *
 *  DETERMINISM: `getMethods()` order is unspecified, so it is only ever counted
 *  or membership-tested, never printed in order. Nothing prints a hash, an
 *  address or a classloader identity.
 */
public class ClassShadowSweep {
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
    interface ThrowingRun { void run() throws Throwable; }

    // ---- fixtures -------------------------------------------------------
    public static class Base {
        public int publicField = 1;
        protected int protectedField = 2;
        private int privateField = 3;
        public void publicMethod() { }
        private void privateMethod() { }
        public Base() { }
        public Base(int x) { }
        private Base(String s) { }
    }
    public static class Derived extends Base { public int own = 4; }
    interface Iface { void m(); }
    enum Colour { RED, GREEN }
    static final Runnable ANON = new Runnable() { public void run() { } };

    // ---- getName / getSimpleName / getPackageName / descriptorString ----
    static void naming() {
        Class<?>[] cs = {
            int.class, long.class, double.class, boolean.class, char.class, void.class,
            int[].class, int[][].class, String.class, String[].class, String[][].class,
            Object.class, Base.class, Derived.class, Iface.class, Colour.class,
            ANON.getClass(), ClassShadowSweep.class,
        };
        for (Class<?> c : cs) {
            String key = "[" + c.getName() + "]";
            p(key + " getName", c.getName());
            p(key + " getSimpleName", c.getSimpleName());
            p(key + " getPackageName", c.getPackageName());
            p(key + " descriptorString", c.descriptorString());
            p(key + " isPrimitive", c.isPrimitive());
            p(key + " isInterface", c.isInterface());
            p(key + " isArray", c.isArray());
            p(key + " getModifiers", Modifier.toString(c.getModifiers()));
            p(key + " getModule non-null", c.getModule() != null);
            p(key + " getModule name", c.getModule().getName());
        }
    }

    // ---- forName --------------------------------------------------------
    static void forName() {
        p("forName String", nameOf(() -> Class.forName("java.lang.String")));
        p("forName array of ref", nameOf(() -> Class.forName("[Ljava.lang.String;")));
        p("forName array of int", nameOf(() -> Class.forName("[I")));
        p("forName 2d int", nameOf(() -> Class.forName("[[I")));
        p("forName nested", nameOf(() -> Class.forName("ClassShadowSweep$Base")));
        // A PRIMITIVE is not findable by name -- "int" is a keyword, not a
        // binary name, and this is the row most from-memory tables get wrong.
        t("forName int", () -> Class.forName("int"));
        t("forName void", () -> Class.forName("void"));
        t("forName missing", () -> Class.forName("no.such.Klass"));
        t("forName empty", () -> Class.forName(""));
        t("forName null", () -> Class.forName(null));
        // Slash form is NOT accepted; the binary name uses dots.
        t("forName slash form", () -> Class.forName("java/lang/String"));
        // Array of a missing element type
        t("forName array of missing", () -> Class.forName("[Lno.such.Klass;"));
        p("forName 3-arg no-init", nameOf(() ->
            Class.forName("java.util.ArrayList", false, ClassShadowSweep.class.getClassLoader())));
        t("forName 3-arg null name", () ->
            Class.forName(null, false, ClassShadowSweep.class.getClassLoader()));
        // A null loader means the BOOTSTRAP loader, which can find String but
        // not an application class.
        p("forName 3-arg boot finds String", nameOf(() ->
            Class.forName("java.lang.String", false, null)));
        t("forName 3-arg boot misses app class", () ->
            Class.forName("ClassShadowSweep", false, null));
    }
    static String nameOf(Callable c) {
        try { return c.call().getName(); }
        catch (Throwable e) { return "THREW " + e.getClass().getName(); }
    }
    interface Callable { Class<?> call() throws Throwable; }

    // ---- cast -----------------------------------------------------------
    static void cast() {
        p("cast ok", String.class.cast("s"));
        // cast(null) is ALWAYS legal, for every class including primitives'
        // wrappers -- it is a no-op that returns null.
        p("cast null on String", String.class.cast(null));
        p("cast null on Integer", Integer.class.cast(null));
        t("cast wrong type", () -> String.class.cast(Integer.valueOf(1)));
        p("cast widening to Object", Object.class.cast("s"));
        p("cast to interface", Runnable.class.cast(ANON) != null);
        t("cast to unrelated interface", () -> Iface.class.cast("s"));
        p("cast array", Object[].class.cast(new String[]{"a"}).length);
        t("cast array wrong element", () -> String[].class.cast(new Integer[]{1}));
        // A PRIMITIVE class cannot cast anything, not even its wrapper.
        t("cast on int.class", () -> int.class.cast(Integer.valueOf(1)));
        p("cast null on int.class", int.class.cast(null));
    }

    // ---- getField / getDeclaredField ------------------------------------
    static void fields() {
        p("getField public", fieldName(Base.class, "publicField", false));
        // getField finds INHERITED public fields; getDeclaredField does not.
        p("getField inherited public", fieldName(Derived.class, "publicField", false));
        p("getDeclaredField own", fieldName(Derived.class, "own", true));
        t("getDeclaredField inherited", () -> Derived.class.getDeclaredField("publicField"));
        // getField finds ONLY public; a protected or private one is invisible.
        t("getField protected", () -> Base.class.getField("protectedField"));
        t("getField private", () -> Base.class.getField("privateField"));
        p("getDeclaredField private", fieldName(Base.class, "privateField", true));
        t("getField missing", () -> Base.class.getField("nope"));
        t("getDeclaredField missing", () -> Base.class.getDeclaredField("nope"));
        t("getField null", () -> Base.class.getField(null));
        t("getDeclaredField null", () -> Base.class.getDeclaredField(null));
        // An array has no declared fields but inherits Object's methods.
        t("getField on array class", () -> int[].class.getField("length"));
        t("getDeclaredField on primitive", () -> int.class.getDeclaredField("x"));
    }
    static String fieldName(Class<?> c, String n, boolean declared) {
        try { return (declared ? c.getDeclaredField(n) : c.getField(n)).getName(); }
        catch (Throwable e) { return "THREW " + e.getClass().getName(); }
    }

    // ---- getMethod / getMethods / constructors ---------------------------
    static void methodsAndCtors() {
        p("getMethod public", methodName(Base.class, "publicMethod"));
        p("getMethod inherited from Object", methodName(Base.class, "toString"));
        t("getMethod private", () -> Base.class.getMethod("privateMethod"));
        t("getMethod missing", () -> Base.class.getMethod("nope"));
        t("getMethod null name", () -> Base.class.getMethod(null));
        // getMethods returns PUBLIC methods including inherited ones, so it must
        // contain Object's. Order is unspecified -- count and membership only.
        Method[] ms = Base.class.getMethods();
        p("getMethods non-empty", ms.length > 0);
        p("getMethods includes toString", hasMethod(ms, "toString"));
        p("getMethods includes publicMethod", hasMethod(ms, "publicMethod"));
        p("getMethods excludes privateMethod", !hasMethod(ms, "privateMethod"));
        p("getMethods on interface includes m", hasMethod(Iface.class.getMethods(), "m"));
        p("getMethods on int.class is empty", int.class.getMethods().length);
        p("getMethods on array length", int[].class.getMethods().length > 0);

        p("getConstructor no-arg", ctor(Base.class, false));
        p("getConstructor int", ctorArgs(Base.class, false, int.class));
        t("getConstructor private", () -> Base.class.getConstructor(String.class));
        p("getDeclaredConstructor private", ctorArgs(Base.class, true, String.class));
        t("getConstructor missing", () -> Base.class.getConstructor(double.class));
        t("getConstructor on interface", () -> Iface.class.getConstructor());
        t("getConstructor on primitive", () -> int.class.getConstructor());
        // A null array means "no parameters", NOT an error.
        p("getConstructor null array", ctorNull(Base.class));
    }
    static boolean hasMethod(Method[] ms, String n) {
        for (Method m : ms) if (m.getName().equals(n)) return true;
        return false;
    }
    static String methodName(Class<?> c, String n) {
        try { return c.getMethod(n).getName(); }
        catch (Throwable e) { return "THREW " + e.getClass().getName(); }
    }
    static String ctor(Class<?> c, boolean declared) {
        try { return String.valueOf((declared ? c.getDeclaredConstructor() : c.getConstructor())
            .getParameterCount()); }
        catch (Throwable e) { return "THREW " + e.getClass().getName(); }
    }
    static String ctorArgs(Class<?> c, boolean declared, Class<?>... ps) {
        try { return String.valueOf((declared ? c.getDeclaredConstructor(ps) : c.getConstructor(ps))
            .getParameterCount()); }
        catch (Throwable e) { return "THREW " + e.getClass().getName(); }
    }
    static String ctorNull(Class<?> c) {
        try { return String.valueOf(c.getConstructor((Class<?>[]) null).getParameterCount()); }
        catch (Throwable e) { return "THREW " + e.getClass().getName(); }
    }

    // ---- enum constants, resources, assertions, Module -------------------
    static void misc() {
        p("enum constants length", Colour.class.getEnumConstants().length);
        p("enum constants first", Colour.class.getEnumConstants()[0]);
        p("enum constants on non-enum", String.class.getEnumConstants());
        p("isEnum on enum", Colour.class.isEnum());
        p("isEnum on String", String.class.isEnum());

        p("getResourceAsStream absent relative", ClassShadowSweep.class
            .getResourceAsStream("no-such-resource.txt"));
        p("getResourceAsStream absent absolute", ClassShadowSweep.class
            .getResourceAsStream("/no/such/resource.txt"));
        p("getResourceAsStream boot class file present", Object.class
            .getResourceAsStream("/java/lang/Object.class") != null);
        t("getResourceAsStream null", () -> ClassShadowSweep.class.getResourceAsStream(null));

        p("desiredAssertionStatus is a boolean",
          ClassShadowSweep.class.desiredAssertionStatus() || true);

        Module base = String.class.getModule();
        Module mine = ClassShadowSweep.class.getModule();
        p("java.base name", base.getName());
        p("unnamed module name", mine.getName());
        p("java.base canRead itself", base.canRead(base));
        p("unnamed canRead java.base", mine.canRead(base));
        p("java.base isExported java.lang", base.isExported("java.lang", mine));
        p("java.base isExported internal to unnamed", base.isExported("jdk.internal.misc", mine));
        p("java.base descriptor name", base.getDescriptor().name());
        p("unnamed descriptor null", mine.getDescriptor());
        p("java.base canUse Runnable", base.canUse(Runnable.class));
        t("canRead null", () -> base.canRead(null));
    }

    public static void main(String[] a) {
        naming();
        forName();
        cast();
        fields();
        methodsAndCtors();
        misc();
        System.out.println("DONE ClassShadowSweep");
    }
}
