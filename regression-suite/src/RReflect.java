import java.lang.annotation.*;
import java.lang.reflect.*;
import java.util.*;

/**
 * Regression: reflection, runtime annotations (read via a dynamic
 * AnnotationProxy — a regressed area), records, and enums.
 */
public class RReflect {
    static int checks = 0;
    static void check(boolean c, String m) { checks++; if (!c) throw new AssertionError(m); }

    @Retention(RetentionPolicy.RUNTIME)
    @interface Tag { String value(); int count() default 1; }

    @Tag(value = "demo", count = 3)
    static class Annotated {
        public int field = 5;
        private String hidden = "h";
        public int times(int x) { return x * field; }
        public Annotated() {}
    }

    record Point(int x, int y) { int sum() { return x + y; } }

    enum Color { RED, GREEN, BLUE }

    public static void main(String[] a) throws Exception {
        Class<Annotated> cls = Annotated.class;
        check(cls.getSimpleName().equals("Annotated"), "getSimpleName");
        check(cls.getName().endsWith("Annotated"), "getName");

        // ---- runtime annotation read (dynamic proxy implementing @Tag) ----
        Tag tag = cls.getAnnotation(Tag.class);
        check(tag != null, "getAnnotation present");
        check(tag.value().equals("demo") && tag.count() == 3, "annotation members via proxy");
        check(cls.isAnnotationPresent(Tag.class), "isAnnotationPresent");
        check(cls.getAnnotations().length >= 1, "getAnnotations");

        // ---- methods / fields / invoke ----
        Method times = cls.getMethod("times", int.class);
        Annotated inst = cls.getDeclaredConstructor().newInstance();
        check((Integer) times.invoke(inst, 4) == 20, "Method.invoke");
        Field f = cls.getField("field");
        check((Integer) f.get(inst) == 5, "Field.get");
        f.set(inst, 10);
        check((Integer) times.invoke(inst, 2) == 20, "Field.set + invoke");
        Field hidden = cls.getDeclaredField("hidden");
        hidden.setAccessible(true);
        check("h".equals(hidden.get(inst)), "private field via setAccessible");
        check(cls.getDeclaredFields().length == 2, "getDeclaredFields count");

        // ---- records ----
        Point p = new Point(3, 4);
        check(p.x() == 3 && p.y() == 4 && p.sum() == 7, "record accessors");
        check(p.equals(new Point(3, 4)) && p.hashCode() == new Point(3, 4).hashCode(), "record equals/hashCode");
        check(p.toString().equals("Point[x=3, y=4]"), "record toString");
        RecordComponent[] rc = Point.class.getRecordComponents();
        check(rc != null && rc.length == 2 && rc[0].getName().equals("x"), "getRecordComponents");

        // ---- enums ----
        check(Color.values().length == 3, "enum values");
        check(Color.valueOf("GREEN") == Color.GREEN && Color.BLUE.ordinal() == 2, "enum valueOf/ordinal");
        EnumMap<Color, Integer> em = new EnumMap<>(Color.class);
        em.put(Color.RED, 1); em.put(Color.BLUE, 3);
        check(em.size() == 2 && em.get(Color.RED) == 1, "EnumMap");

        // ---- isInstance / cast / array reflection ----
        check(Number.class.isInstance(Integer.valueOf(1)), "isInstance");
        check(CharSequence.class.cast("hi").length() == 2, "Class.cast");
        Object arr = Array.newInstance(int.class, 3);
        Array.setInt(arr, 1, 42);
        check(Array.getInt(arr, 1) == 42 && Array.getLength(arr) == 3, "Array reflection");
        check(int[].class.isArray() && int[].class.getComponentType() == int.class, "array class");

        // ---- getSimpleName / getCanonicalName on nested / top-level / array /
        // anonymous classes (SBR-07). The nested type is resolved via
        // Class.forName WITHOUT ever referencing its enclosing class in source,
        // so the outer class is NOT loaded when getSimpleName runs — the exact
        // shape (Kotlin's protobuf ProtoBuf$StringTable) that used to return the
        // binary leaf "RReflectOuter$Nested" instead of "Nested" because
        // getDeclaringClass0() couldn't see the unloaded outer.
        Class<?> nested = Class.forName("RReflectOuter$Nested");
        check(nested.getSimpleName().equals("Nested"), "nested getSimpleName (outer unloaded)");
        check(nested.getCanonicalName().equals("RReflectOuter.Nested"), "nested getCanonicalName");
        check(nested.getName().equals("RReflectOuter$Nested"), "nested getName");
        // array of the (still-unloaded-outer) nested type -> "Nested[]"
        Object nestedArr = Array.newInstance(nested, 0);
        check(nestedArr.getClass().getSimpleName().equals("Nested[]"), "nested-array getSimpleName");
        // top-level
        check(String.class.getSimpleName().equals("String"), "top-level getSimpleName");
        check(String.class.getCanonicalName().equals("java.lang.String"), "top-level getCanonicalName");
        // primitive array
        check(int[].class.getSimpleName().equals("int[]"), "primitive-array getSimpleName");
        check(int[].class.getCanonicalName().equals("int[]"), "primitive-array getCanonicalName");
        // member (nested) class: keeps both a simple and a canonical name.
        check(cls.getCanonicalName().equals("RReflect.Annotated"), "member getCanonicalName");
        // anonymous class: simple name is "", canonical name is null. The
        // InnerClasses entry of an anonymous class has inner_name_index == 0,
        // so the simple name must NOT fall back to the `$`-split binary tail
        // (the compiler's ordinal, e.g. "1").
        Object anon = new Object() {};
        check(anon.getClass().getSimpleName().isEmpty(), "anonymous getSimpleName empty");
        check(anon.getClass().getCanonicalName() == null, "anonymous getCanonicalName null");
        check(anon.getClass().getName().startsWith("RReflect$"), "anonymous getName");
        // an ARRAY of an anonymous class inherits both: simple name is just
        // the "[]" suffix, and there is still no canonical name.
        Object anonArr = Array.newInstance(anon.getClass(), 0);
        check(anonArr.getClass().getSimpleName().equals("[]"), "anonymous-array getSimpleName");
        check(anonArr.getClass().getCanonicalName() == null, "anonymous-array getCanonicalName null");
        // anonymous implementing an interface behaves identically.
        Runnable anonRun = new Runnable() { public void run() {} };
        check(anonRun.getClass().getSimpleName().isEmpty(), "anonymous(iface) getSimpleName empty");
        check(anonRun.getClass().getCanonicalName() == null, "anonymous(iface) getCanonicalName null");
        // local (declared-in-a-method) class: outer_class_info_index == 0 but
        // inner_name_index != 0 -> it HAS a simple name and has NO canonical name.
        class RReflectLocal {}
        check(RReflectLocal.class.getSimpleName().equals("RReflectLocal"), "local getSimpleName");
        check(RReflectLocal.class.getCanonicalName() == null, "local getCanonicalName null");

        System.out.println("PASS RReflect (" + checks + " checks)");
    }
}

/**
 * Sibling top-level helper for the SBR-07 nested-name checks. Deliberately
 * referenced ONLY via {@code Class.forName("RReflectOuter$Nested")} so its
 * enclosing class stays unloaded when {@code getSimpleName()} runs.
 */
class RReflectOuter {
    static class Nested {}
}
