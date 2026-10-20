import java.lang.reflect.*;
import java.util.*;
import java.util.function.*;

/**
 * Interpreter round i4 wave 98, lane L3: declared members as HotSpot 25 reports
 * them -- field ORDER (class-file order, which HotSpot keeps), the CONTENTS of
 * the method and constructor lists (sorted here, because HotSpot's method
 * order follows symbol addresses and is not stable), synthetic / bridge /
 * lambda methods, Modifier bits, toString / toGenericString, generic
 * signatures, varargs, default methods, and enum / inner / record synthetics.
 */
public class L3W98ReflectMembers {
    interface Shape<T extends Comparable<T>> {
        T area();
        default String label() { return "shape"; }
        static int count() { return 1; }
    }

    static abstract class Base<T> {
        abstract T get();
        protected Object self() { return this; }
        <U extends Number & Comparable<U>> List<? super U> widen(Map<String, ? extends U> m, U... us) { return null; }
    }

    static final class Impl extends Base<String> implements Shape<Integer>, Comparable<Impl> {
        private static final long serialVersionUID = 1L;
        transient volatile int t;
        protected static String s = "s";
        public final double d = 1.0;
        int[][] grid;
        List<? extends Number>[] lists;

        @Override String get() { return "x"; }
        @Override protected Impl self() { return this; }
        @Override public Integer area() { return 3; }
        @Override public int compareTo(Impl o) { return 0; }
        synchronized native void nat();
        strictfp double fp() { return 0; }
        public static <E extends Enum<E>> E first(Class<E> c) throws IllegalStateException, java.io.IOException { return null; }
        Runnable lambda() { return () -> System.out.println(t); }
        Supplier<String> lambda2() { return () -> s; }
    }

    enum Color { RED, GREEN { @Override int v() { return 2; } }; int v() { return 1; } }

    record Point(int x, List<String> tags) implements Comparable<Point> {
        Point { Objects.requireNonNull(tags); }
        public int compareTo(Point o) { return 0; }
    }

    class Inner {
        int v;
        Inner(int v) { this.v = v; }
    }

    static String sortedMethods(Class<?> c) {
        List<String> out = new ArrayList<>();
        for (Method m : c.getDeclaredMethods()) {
            String name = m.getName();
            if (name.startsWith("lambda$")) {
                // lambda$<method>$<n>: the index is javac's, stable for this source.
            }
            out.add(Modifier.toString(m.getModifiers()) + " [" + Integer.toHexString(m.getModifiers())
                    + (m.isSynthetic() ? " synthetic" : "") + (m.isBridge() ? " bridge" : "")
                    + (m.isVarArgs() ? " varargs" : "") + (m.isDefault() ? " default" : "") + "] "
                    + m.toGenericString());
        }
        Collections.sort(out);
        return String.join("\n    ", out);
    }

    static String sortedCtors(Class<?> c) {
        List<String> out = new ArrayList<>();
        for (Constructor<?> k : c.getDeclaredConstructors()) {
            out.add("[" + Integer.toHexString(k.getModifiers()) + (k.isSynthetic() ? " synthetic" : "")
                    + (k.isVarArgs() ? " varargs" : "") + "] " + k.toGenericString()
                    + " params=" + k.getParameterCount() + " generic=" + Arrays.toString(k.getGenericParameterTypes()));
        }
        Collections.sort(out);
        return String.join("\n    ", out);
    }

    static String fields(Class<?> c) {
        List<String> out = new ArrayList<>();
        for (Field f : c.getDeclaredFields()) {
            out.add("[" + Integer.toHexString(f.getModifiers()) + (f.isSynthetic() ? " synthetic" : "")
                    + (f.isEnumConstant() ? " enum" : "") + "] " + f.toGenericString());
        }
        return String.join("\n    ", out); // class-file order: NOT sorted
    }

    static void dump(Class<?> c) {
        System.out.println("== " + c.getName() + " mods=" + Integer.toHexString(c.getModifiers())
                + " access=" + Modifier.toString(c.getModifiers()));
        System.out.println("  fields:\n    " + fields(c));
        System.out.println("  methods:\n    " + sortedMethods(c));
        System.out.println("  ctors:\n    " + sortedCtors(c));
        System.out.println("  generic super: " + c.getGenericSuperclass());
        System.out.println("  generic ifaces: " + Arrays.toString(c.getGenericInterfaces()));
        System.out.println("  type params: " + Arrays.toString(c.getTypeParameters()));
        System.out.println("  toGenericString: " + c.toGenericString());
    }

    public static void main(String[] args) throws Exception {
        dump(Shape.class);
        dump(Base.class);
        dump(Impl.class);
        dump(Color.class);
        dump(Color.GREEN.getClass());
        dump(Point.class);
        dump(Inner.class);

        Method widen = Base.class.getDeclaredMethod("widen", Map.class, Number[].class);
        System.out.println("widen generic return: " + widen.getGenericReturnType());
        System.out.println("widen generic params: " + Arrays.toString(widen.getGenericParameterTypes()));
        TypeVariable<Method> u = widen.getTypeParameters()[0];
        System.out.println("widen U bounds: " + Arrays.toString(u.getBounds()) + " decl=" + u.getGenericDeclaration());
        Method first = Impl.class.getDeclaredMethod("first", Class.class);
        System.out.println("first exceptions: " + Arrays.toString(first.getExceptionTypes())
                + " generic=" + Arrays.toString(first.getGenericExceptionTypes()));
        System.out.println("public methods of Impl named self: " + Arrays.stream(Impl.class.getMethods())
                .filter(m -> m.getName().equals("self")).count());
        System.out.println("getMethod(area) return: " + Impl.class.getMethod("area").getReturnType());
        System.out.println("getMethod(compareTo,Object) bridge: " + Impl.class.getMethod("compareTo", Object.class).isBridge());
        System.out.println("getMethod(label) declaring: " + Impl.class.getMethod("label").getDeclaringClass().getName());
        try {
            Impl.class.getMethod("count");
            System.out.println("static iface method visible: yes");
        } catch (NoSuchMethodException e) {
            System.out.println("static iface method visible: NoSuchMethodException " + e.getMessage());
        }
        Field grid = Impl.class.getDeclaredField("grid");
        System.out.println("grid type: " + grid.getType() + " / " + grid.getType().getTypeName());
        Field lists = Impl.class.getDeclaredField("lists");
        System.out.println("lists generic: " + lists.getGenericType() + " class=" + lists.getGenericType().getClass().getInterfaces()[0].getSimpleName());
        Parameter[] ps = Impl.class.getDeclaredMethod("first", Class.class).getParameters();
        System.out.println("param: " + ps[0] + " named=" + ps[0].isNamePresent() + " implicit=" + ps[0].isImplicit());
        Parameter[] ip = Inner.class.getDeclaredConstructor(L3W98ReflectMembers.class, int.class).getParameters();
        for (Parameter p : ip) {
            System.out.println("inner ctor param: " + p + " mods=" + Integer.toHexString(p.getModifiers())
                    + " implicit=" + p.isImplicit() + " synthetic=" + p.isSynthetic());
        }
    }
}
