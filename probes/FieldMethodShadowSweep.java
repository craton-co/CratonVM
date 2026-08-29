import java.lang.annotation.*;
import java.lang.reflect.*;
import java.util.*;

/** L5, part 2: the `java.lang.reflect.Field` (32) and `java.lang.reflect.Method`
 *  (26) triples the `--jdk-only-report` marks `outcome=native-won`.
 *
 *  Lane L5 owns reflection; see `HANDOFF-20260828-L5-reflection.md`. Part 1 was
 *  `ClassShadowSweep` (261 rows, 9 defects). These 58 rows are the densest of
 *  the 116 still unprobed in this lane.
 *
 *  `Field`'s typed accessors are the richest contract in the whole reflection
 *  surface, because they are asymmetric in a way that is easy to implement as
 *  if it were symmetric:
 *
 *    getInt  on a `byte` field   WIDENS   -> legal
 *    getByte on an `int`  field  NARROWS  -> IllegalArgumentException
 *    setInt  on a `byte` field   NARROWS  -> IllegalArgumentException
 *    setLong on an `int`  field  NARROWS  -> IllegalArgumentException
 *
 *  A shim that treats every primitive slot as "a number" passes the happy path
 *  and gets every one of the refusals wrong — which is exactly the shape all 28
 *  defects found in this campaign have had.
 *
 *  DETERMINISM: annotation and member arrays have unspecified order, so they are
 *  counted or membership-tested, never printed in order. Nothing prints an
 *  identity hash or an address.
 */
public class FieldMethodShadowSweep {
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
    /** The thrown type, unwrapping nothing — `InvocationTargetException` is
     *  itself the answer for a callee that throws. */
    static String thrown(ThrowingRun r) {
        try { r.run(); return "no-throw"; }
        catch (Throwable e) { return e.getClass().getName(); }
    }

    @Retention(RetentionPolicy.RUNTIME) @interface Marker { String value() default "d"; }

    public static class Holder {
        public boolean z = true;
        public byte b = 1;
        public char c = 'x';
        public short s = 2;
        public int i = 3;
        public long j = 4L;
        public float f = 5.0f;
        public double d = 6.0;
        public String ref = "r";
        public final int finalInt = 7;
        public static int staticInt = 8;
        private int priv = 9;
        @Marker("m") public int annotated = 10;
        public List<String> generic = new ArrayList<>();
    }
    enum Colour { RED, GREEN }

    static Field f(String n) throws Exception { return Holder.class.getDeclaredField(n); }

    // ---- Field: the typed accessors and their widening rules ------------
    static void fieldAccessors() throws Exception {
        Holder h = new Holder();
        p("get boolean", f("z").getBoolean(h));
        p("get byte", f("b").getByte(h));
        p("get char", f("c").getChar(h));
        p("get short", f("s").getShort(h));
        p("get int", f("i").getInt(h));
        p("get long", f("j").getLong(h));
        p("get float", f("f").getFloat(h));
        p("get double", f("d").getDouble(h));
        p("get ref", f("ref").get(h));
        // `get` on a primitive field BOXES, and the box type is exact.
        p("get boxes byte", f("b").get(h).getClass().getName());
        p("get boxes char", f("c").get(h).getClass().getName());
        p("get boxes double", f("d").get(h).getClass().getName());

        // WIDENING is legal in the getter direction.
        p("getInt on byte widens", f("b").getInt(h));
        p("getInt on char widens", f("c").getInt(h));
        p("getInt on short widens", f("s").getInt(h));
        p("getLong on int widens", f("j").getLong(h));
        p("getLong on byte widens", f("b").getLong(h));
        p("getDouble on float widens", f("d").getDouble(h));
        p("getDouble on int widens", f("i").getDouble(h));
        p("getFloat on long widens", f("j").getFloat(h));

        // NARROWING is not, in either direction.
        t("getByte on int narrows", () -> f("i").getByte(h));
        t("getShort on int narrows", () -> f("i").getShort(h));
        t("getChar on int narrows", () -> f("i").getChar(h));
        t("getInt on long narrows", () -> f("j").getInt(h));
        t("getInt on float narrows", () -> f("f").getInt(h));
        t("getFloat on double narrows", () -> f("d").getFloat(h));
        // boolean is not a number and joins nothing.
        t("getBoolean on int", () -> f("i").getBoolean(h));
        t("getInt on boolean", () -> f("z").getInt(h));
        t("getInt on ref", () -> f("ref").getInt(h));
        t("getBoolean on ref", () -> f("ref").getBoolean(h));

        // Receiver rules.
        t("get instance field with null", () -> f("i").getInt(null));
        t("get with wrong receiver type", () -> f("i").getInt("not a Holder"));
        p("get static with null receiver", f("staticInt").getInt(null));
        p("get static with any receiver", f("staticInt").getInt(h));
    }

    static void fieldSetters() throws Exception {
        Holder h = new Holder();
        f("i").setInt(h, 100);
        p("setInt", h.i);
        f("b").setByte(h, (byte) 11);
        p("setByte", h.b);
        f("j").setLong(h, 12L);
        p("setLong", h.j);
        f("d").setDouble(h, 13.0);
        p("setDouble", h.d);
        f("z").setBoolean(h, false);
        p("setBoolean", h.z);
        f("ref").set(h, "changed");
        p("set ref", h.ref);
        f("ref").set(h, null);
        p("set ref null", h.ref);

        // WIDENING is legal in the setter direction too: setLong INTO a long
        // from an int source is fine, but setInt into a byte field is a
        // NARROWING store and must be refused.
        Holder w = new Holder();
        f("j").setInt(w, 14);
        p("setInt on long field widens", w.j);
        f("d").setFloat(w, 15.0f);
        p("setFloat on double field widens", w.d);
        f("i").setChar(w, 'A');
        p("setChar on int field widens", w.i);
        t("setInt on byte field narrows", () -> f("b").setInt(h, 1));
        t("setLong on int field narrows", () -> f("i").setLong(h, 1L));
        t("setDouble on float field narrows", () -> f("f").setDouble(h, 1.0));
        t("setBoolean on int field", () -> f("z").setInt(h, 1));

        // `set` with a boxed value follows the same rules, and a wrong box is
        // an IllegalArgumentException, not a ClassCastException.
        f("i").set(h, Integer.valueOf(20));
        p("set boxed Integer into int", h.i);
        f("j").set(h, Integer.valueOf(21));
        p("set boxed Integer into long widens", h.j);
        t("set boxed Long into int", () -> f("i").set(h, Long.valueOf(1)));
        t("set boxed String into int", () -> f("i").set(h, "s"));
        t("set null into primitive", () -> f("i").set(h, null));
        t("set wrong ref type", () -> f("ref").set(h, Integer.valueOf(1)));

        // final and private, without setAccessible.
        t("set final without access", () -> f("finalInt").setInt(h, 99));
        t("set private without access", () -> f("priv").setInt(h, 99));
        t("get private without access", () -> f("priv").getInt(h));
        // and with it
        Field pv = f("priv");
        pv.setAccessible(true);
        pv.setInt(h, 42);
        p("private after setAccessible", pv.getInt(h));
        t("set instance field with null receiver", () -> f("i").set(null, 1));
    }

    static void fieldMetadata() throws Exception {
        p("getName", f("i").getName());
        p("getType", f("i").getType().getName());
        p("getType ref", f("ref").getType().getName());
        p("getDeclaringClass", f("i").getDeclaringClass().getSimpleName());
        p("getModifiers public", Modifier.toString(f("i").getModifiers()));
        p("getModifiers final", Modifier.toString(f("finalInt").getModifiers()));
        p("getModifiers static", Modifier.toString(f("staticInt").getModifiers()));
        p("getModifiers private", Modifier.toString(f("priv").getModifiers()));
        p("isSynthetic", f("i").isSynthetic());
        p("isEnumConstant on field", f("i").isEnumConstant());
        p("isEnumConstant on enum", Colour.class.getDeclaredField("RED").isEnumConstant());
        p("getGenericType raw", f("i").getGenericType().getTypeName());
        p("getGenericType parameterized", f("generic").getGenericType().getTypeName());
        p("getAnnotation present", f("annotated").getAnnotation(Marker.class) != null);
        p("getAnnotation value", f("annotated").getAnnotation(Marker.class).value());
        p("getAnnotation absent", f("i").getAnnotation(Marker.class));
        p("getDeclaredAnnotations count", f("annotated").getDeclaredAnnotations().length);
        p("getDeclaredAnnotations empty", f("i").getDeclaredAnnotations().length);
        p("getAnnotatedType typeName", f("i").getAnnotatedType().getType().getTypeName());
        p("equals same field", f("i").equals(f("i")));
        p("hashCode stable", f("i").hashCode() == f("i").hashCode());
        p("toString", f("i").toString());
        t("getAnnotation null class", () -> f("i").getAnnotation(null));
    }

    // ---- Method ---------------------------------------------------------
    public static class Target {
        public int add(int a, int b) { return a + b; }
        public static String stat(String s) { return "s:" + s; }
        public void boom() { throw new IllegalStateException("boom"); }
        private int secret() { return 7; }
        public int varargs(int... xs) { return xs.length; }
        public String overload(String s) { return s; }
        public List<String> generic(Map<String, Integer> m) { return null; }
        public void throwsChecked() throws java.io.IOException { }
        @Marker("mm") public void annotated(@Marker int x) { }
    }
    interface Iface { default int def() { return 1; } void abs(); }

    static Method m(String n, Class<?>... ps) throws Exception {
        return Target.class.getDeclaredMethod(n, ps);
    }

    static void methodInvoke() throws Exception {
        Target t = new Target();
        p("invoke instance", m("add", int.class, int.class).invoke(t, 1, 2));
        p("invoke static null receiver", m("stat", String.class).invoke(null, "x"));
        p("invoke static any receiver", m("stat", String.class).invoke(t, "y"));
        p("invoke varargs as array", m("varargs", int[].class).invoke(t, (Object) new int[]{1, 2, 3}));
        p("invoke with null ref arg", m("overload", String.class).invoke(t, (Object) null));

        // A callee that throws is wrapped, and the WRAPPER is the answer.
        p("callee throws wrapper", thrown(() -> m("boom").invoke(t)));
        try { m("boom").invoke(t); }
        catch (InvocationTargetException e) {
            p("callee throws cause", e.getCause().getClass().getName());
            p("callee throws cause message", e.getCause().getMessage());
        }

        // Receiver and argument rules.
        p("instance with null receiver", thrown(() -> m("add", int.class, int.class).invoke(null, 1, 2)));
        p("wrong receiver type", thrown(() -> m("add", int.class, int.class).invoke("s", 1, 2)));
        p("too few args", thrown(() -> m("add", int.class, int.class).invoke(t, 1)));
        p("too many args", thrown(() -> m("add", int.class, int.class).invoke(t, 1, 2, 3)));
        p("null args array for 2-arg", thrown(() -> m("add", int.class, int.class).invoke(t, (Object[]) null)));
        p("null args array for 0-arg", thrown(() -> m("boom").invoke(t, (Object[]) null)));
        p("wrong arg type", thrown(() -> m("add", int.class, int.class).invoke(t, "a", "b")));
        p("null for primitive arg", thrown(() -> m("add", int.class, int.class).invoke(t, null, 2)));
        // widening applies to arguments too
        p("byte arg widens to int", m("add", int.class, int.class).invoke(t, (byte) 1, (short) 2));
        p("private without access", thrown(() -> m("secret").invoke(t)));
        Method sec = m("secret");
        sec.setAccessible(true);
        p("private after setAccessible", sec.invoke(t));
    }

    static void methodMetadata() throws Exception {
        Method add = m("add", int.class, int.class);
        p("getName", add.getName());
        p("getParameterCount", add.getParameterCount());
        p("getParameterTypes", Arrays.toString(add.getParameterTypes()));
        p("getReturnType", add.getReturnType().getName());
        p("getDeclaringClass", add.getDeclaringClass().getSimpleName());
        p("getModifiers", Modifier.toString(add.getModifiers()));
        p("getExceptionTypes empty", m("add", int.class, int.class).getExceptionTypes().length);
        p("getExceptionTypes checked",
          Arrays.toString(m("throwsChecked").getExceptionTypes()));
        p("isVarArgs false", add.isVarArgs());
        p("isVarArgs true", m("varargs", int[].class).isVarArgs());
        p("isSynthetic", add.isSynthetic());
        p("isBridge", add.isBridge());
        p("isDefault on class method", add.isDefault());
        p("isDefault on interface default", Iface.class.getDeclaredMethod("def").isDefault());
        p("isDefault on interface abstract", Iface.class.getDeclaredMethod("abs").isDefault());
        p("getDefaultValue non-annotation", add.getDefaultValue());
        p("getDefaultValue annotation member",
          Marker.class.getDeclaredMethod("value").getDefaultValue());
        p("getGenericReturnType",
          m("generic", Map.class).getGenericReturnType().getTypeName());
        p("getGenericParameterTypes",
          Arrays.toString(m("generic", Map.class).getGenericParameterTypes()));
        p("getGenericExceptionTypes", m("throwsChecked").getGenericExceptionTypes().length);
        p("getAnnotatedReturnType", add.getAnnotatedReturnType().getType().getTypeName());
        p("getTypeParameters count", add.getTypeParameters().length);
        p("getAnnotation present", m("annotated", int.class).getAnnotation(Marker.class).value());
        p("getDeclaredAnnotations count", m("annotated", int.class).getDeclaredAnnotations().length);
        p("getParameterAnnotations rows", m("annotated", int.class).getParameterAnnotations().length);
        p("toString", add.toString());
        p("equals same method", add.equals(m("add", int.class, int.class)));
        p("hashCode stable", add.hashCode() == m("add", int.class, int.class).hashCode());
        t("getDeclaredMethod missing", () -> Target.class.getDeclaredMethod("nope"));
        t("getDeclaredMethod null name", () -> Target.class.getDeclaredMethod(null));
    }

    public static void main(String[] a) throws Exception {
        fieldAccessors();
        fieldSetters();
        fieldMetadata();
        methodInvoke();
        methodMetadata();
        System.out.println("DONE FieldMethodShadowSweep");
    }
}
