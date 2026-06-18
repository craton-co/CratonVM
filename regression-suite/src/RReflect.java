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

        System.out.println("PASS RReflect (" + checks + " checks)");
    }
}
