// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.annotation.Annotation;
import java.lang.annotation.Documented;
import java.lang.annotation.ElementType;
import java.lang.annotation.Inherited;
import java.lang.annotation.Repeatable;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
import java.lang.reflect.Proxy;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.EnumMap;
import java.util.EnumSet;
import java.util.List;
import java.util.Map;
import java.util.concurrent.TimeUnit;
import java.util.function.Supplier;

/**
 * SBR wave 6, lane {@code classrefl}: the {@code java.lang.Class} reflection
 * surface -- the member lookups ({@code getMethod}, {@code getField},
 * {@code getConstructor}, {@code getDeclaredField(s)}, {@code getDeclaredConstructor}),
 * the name accessors ({@code getSimpleName}, {@code getCanonicalName},
 * {@code getTypeName}, {@code descriptorString}), the shape accessors
 * ({@code getModifiers}, {@code isInterface}, {@code isPrimitive}, {@code isArray}),
 * the class-level annotation getters, {@code getEnumConstants} /
 * {@code Enum.valueOf} ({@code getEnumConstantsShared}) and {@code newInstance}.
 *
 * <p>Every row is evaluated ONCE and printed (the first-run value), re-evaluated
 * inside a hot loop and compared with that first value ({@code drift}), and once
 * more after a batch of forced collections ({@code gcdrift}: the reflection and
 * annotation caches hang off soft references and side stores). Diff the whole
 * output against HotSpot 25. Nothing printed depends on identity hashes.
 *
 * <pre>
 *   java SbrW6Classrefl                       (HotSpot oracle)
 *   cratonvm --jdk-only -cp . SbrW6Classrefl
 * </pre>
 */
public class SbrW6Classrefl {
    static final List<String> LABELS = new ArrayList<>();
    static final List<Supplier<String>> ROWS = new ArrayList<>();

    static void row(String label, Supplier<String> s) {
        LABELS.add(label);
        ROWS.add(s);
    }

    interface X {
        Object get() throws Throwable;
    }

    static void rowx(String label, X x) {
        row(label, () -> {
            try {
                return String.valueOf(x.get());
            } catch (InvocationTargetException e) {
                Throwable c = e.getCause();
                return "!ITE(" + c.getClass().getName() + ": " + c.getMessage() + ")";
            } catch (Throwable t) {
                return "!" + t.getClass().getName() + ": " + t.getMessage();
            }
        });
    }

    static String safe(Supplier<String> s) {
        try {
            return s.get();
        } catch (Throwable t) {
            return "!" + t.getClass().getName() + ": " + t.getMessage();
        }
    }

    static String units(String s) {
        if (s == null) return "null";
        StringBuilder b = new StringBuilder();
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.toString();
    }

    static String mask(String s) {
        return s == null ? "null" : s.replaceAll("\\$Proxy[0-9]+", "\\$ProxyN").replaceAll("jdk\\.proxy[0-9]+", "jdk.proxyN")
                .replaceAll("\\$\\$Lambda[$/.0-9a-fx]*", "\\$\\$LambdaN").replaceAll("0x[0-9a-f]+", "0xH")
                .replaceAll("@[0-9a-f]{4,}", "@H");
    }

    // ---------------------------------------------------------------- fixtures

    @Retention(RetentionPolicy.RUNTIME)
    @Inherited
    @interface Inh {
        String value() default "inh";
    }

    @Retention(RetentionPolicy.RUNTIME)
    @interface Plain {
        int n() default 1;

        Class<?> k() default Object.class;

        String[] s() default {"a", "b"};

        TimeUnit u() default TimeUnit.SECONDS;
    }

    @Retention(RetentionPolicy.RUNTIME)
    @Repeatable(Reps.class)
    @interface Rep {
        int value();
    }

    @Retention(RetentionPolicy.RUNTIME)
    @interface Reps {
        Rep[] value();
    }

    @Retention(RetentionPolicy.CLASS)
    @interface Cls {}

    @Retention(RetentionPolicy.SOURCE)
    @interface Src {}

    @Retention(RetentionPolicy.RUNTIME)
    @Target(ElementType.TYPE_USE)
    @interface TU {}

    @Inh("base")
    @Plain(n = 5, k = String.class, s = {"x"}, u = TimeUnit.DAYS)
    @Rep(1)
    @Rep(2)
    @Cls
    @Src
    public static class Base implements Marker {
        public int pubF = 1;
        protected int protF = 2;
        int pkgF = 3;
        private int privF = 4;
        public static int STATIC_F = 5;
        public static final String CONST = "c";

        public Base() {}

        public Base(int a) {}

        protected Base(String s) {}

        private Base(long l) {}

        public void abs() {}

        public void pub() {}

        public void pub(int a) {}

        public void pub(int a, String b) {}

        public static String stat(Object o) {
            return "stat";
        }

        protected void prot() {}

        void pkg() {}

        private void priv() {}

        public Object cov() {
            return null;
        }

        public void varargs(String... a) {}

        public void prims(int a, long b, double c, boolean d, byte e, char f, short g, float h) {}

        public void arrs(int[] a, String[][] b) {}
    }

    public interface Marker {
        int MARK = 9;

        default void dflt() {}

        void abs();
    }

    interface PkgIface {
        String P = "p";
    }

    @Plain
    public static class Derived extends Base implements PkgIface {
        public int derivedF = 6;

        public Derived() {}

        @Override
        public String cov() {
            return "d";
        }

        @Override
        public void abs() {}
    }

    public static abstract class Abs {
        public abstract void m();

        public Abs() {}
    }

    public static class Thrower {
        public Thrower() throws Exception {
            throw new Exception("checked");
        }
    }

    public static class UThrower {
        public UThrower() {
            throw new IllegalStateException("unchecked");
        }
    }

    public static class NoDefault {
        public NoDefault(int a) {}
    }

    private static class PrivNested {}

    protected static class ProtNested {}

    static class PkgNested {}

    public final class Inner {
        public Inner() {}

        public int f;
    }

    public enum Color {
        RED, GREEN {
            @Override
            public String toString() {
                return "g";
            }
        },
        BLUE;
    }

    public record Rec(int a, String b) {}

    @Plain
    public @interface Anno2 {}

    enum Empty {}

    public static class Dollar$Name {}

    // ---------------------------------------------------------------- helpers

    static String ms(Method m) {
        return m == null ? "null" : m.toString();
    }

    static String all(Object[] a) {
        if (a == null) return "null";
        StringBuilder b = new StringBuilder("[");
        for (Object o : a) b.append(o).append("; ");
        return b.append("]").toString();
    }

    static String anns(Annotation[] a) {
        if (a == null) return "null";
        String[] s = new String[a.length];
        for (int i = 0; i < a.length; i++) s[i] = an(a[i]);
        Arrays.sort(s);
        return Arrays.toString(s);
    }

    /**
     * A {@code Plain} annotation prints its members in accessor order: the JDK orders them by
     * {@code Class.getDeclaredMethods()} on the annotation type, which is HotSpot symbol-address
     * order and cannot be reproduced (four rows of the first run differed on nothing else).
     */
    static String an(Annotation a) {
        if (a instanceof Plain p) {
            return "@Plain[" + p.n() + "," + p.k().getName() + "," + Arrays.toString(p.s()) + "," + p.u() + "]";
        }
        return String.valueOf(a);
    }

    static String names(Class<?> c) {
        return c.getName() + " | " + c.getSimpleName() + " | " + c.getCanonicalName() + " | " + c.getTypeName() + " | "
                + c.descriptorString();
    }

    static String shape(Class<?> c) {
        return Integer.toHexString(c.getModifiers()) + " " + Modifier.toString(c.getModifiers()) + " i=" + c.isInterface() + " p="
                + c.isPrimitive() + " a=" + c.isArray() + " an=" + c.isAnnotation() + " e=" + c.isEnum() + " s=" + c.isSynthetic()
                + " r=" + c.isRecord() + " h=" + c.isHidden();
    }

    static Class<?> lambdaClass() {
        Runnable r = () -> {};
        return r.getClass();
    }

    static Class<?> anon() {
        return new Object() {}.getClass();
    }

    static Class<?> local() {
        class Loc {}
        return Loc.class;
    }

    static Class<?> localRecord() {
        record LR(int a) {}
        return LR.class;
    }

    static Class<?> proxy() {
        return Proxy.newProxyInstance(Marker.class.getClassLoader(), new Class<?>[] {Marker.class}, (p, m, a) -> null).getClass();
    }

    public static void main(String[] args) throws Throwable {
        final Class<?> b = Base.class;
        final Class<?> d = Derived.class;
        final Class<?>[] shapes = {
            Object.class, String.class, Runnable.class, Marker.class, PkgIface.class, Base.class, Derived.class, Abs.class,
            PrivNested.class, ProtNested.class, PkgNested.class, Inner.class, Color.class, Color.GREEN.getClass(), Rec.class,
            Anno2.class, Empty.class, Dollar$Name.class, Map.Entry.class, java.util.AbstractMap.SimpleEntry.class,
            Thread.State.class, Character.UnicodeBlock.class, java.util.concurrent.ConcurrentHashMap.class, Override.class,
            Deprecated.class, int.class, long.class, boolean.class, void.class, char.class, byte.class, short.class,
            float.class, double.class, int[].class, int[][].class, Object[].class, String[][][].class, PrivNested[].class,
            ProtNested[].class, PkgNested[][].class, Marker[].class, Color[].class, boolean[].class, Class.class,
            Class[].class, Void.class, Integer.class, Enum.class, Number.class, java.io.Serializable.class,
            java.util.List.class, java.util.ArrayList.class, java.lang.reflect.Modifier.class, Proxy.class,
            java.io.File.class, java.util.HashMap.class
        };
        // ------------------------------------------------ names, shapes
        for (Class<?> c : shapes) {
            final Class<?> cc = c;
            rowx("names " + c.getName(), () -> names(cc));
            rowx("shape " + c.getName(), () -> shape(cc));
        }
        rowx("names lambda", () -> names(lambdaClass()));
        rowx("shape lambda", () -> shape(lambdaClass()));
        rowx("names lambda array", () -> names(lambdaClass().arrayType()));
        rowx("names anonymous", () -> names(anon()));
        rowx("shape anonymous", () -> shape(anon()));
        rowx("names local", () -> names(local()));
        rowx("shape local", () -> shape(local()));
        rowx("names local array", () -> names(local().arrayType()));
        rowx("names local record", () -> names(localRecord()));
        rowx("shape local record", () -> shape(localRecord()));
        rowx("names proxy", () -> names(proxy()));
        rowx("shape proxy", () -> shape(proxy()));
        rowx("names proxy array", () -> names(proxy().arrayType()));
        rowx("names enum body array", () -> names(Color.GREEN.getClass().arrayType()));
        rowx("names 5-dim", () -> names(int[][][][][].class));
        rowx("descriptor forName roundtrip", () -> {
            StringBuilder sb = new StringBuilder();
            for (Class<?> c : new Class<?>[] {String[].class, int[][].class, Base.class, PrivNested[].class}) {
                sb.append(Class.forName(c.getName()) == c).append(' ');
            }
            return sb;
        });
        rowx("arrayType/componentType", () -> {
            StringBuilder sb = new StringBuilder();
            for (Class<?> c : new Class<?>[] {int.class, String.class, int[].class, Base.class}) {
                sb.append(c.arrayType().getName()).append('/').append(c.componentType()).append('/').append(c.getComponentType())
                        .append(' ');
            }
            return sb;
        });
        rowx("getModifiers via Modifier on nested", () -> {
            StringBuilder sb = new StringBuilder();
            for (Class<?> c : new Class<?>[] {Base.class, PrivNested.class, ProtNested.class, PkgNested.class, Inner.class, Marker.class}) {
                sb.append(Modifier.isStatic(c.getModifiers())).append(Modifier.isPublic(c.getModifiers()))
                        .append(Modifier.isPrivate(c.getModifiers())).append(Modifier.isProtected(c.getModifiers()))
                        .append(Modifier.isAbstract(c.getModifiers())).append(Modifier.isFinal(c.getModifiers())).append(' ');
            }
            return sb;
        });
        rowx("accessFlags", () -> Base.class.accessFlags() + " " + PrivNested.class.accessFlags() + " " + int[].class.accessFlags()
                + " " + int.class.accessFlags() + " " + Marker.class.accessFlags());
        // ------------------------------------------------ getMethod
        rowx("getMethod pub()", () -> ms(b.getMethod("pub")));
        rowx("getMethod pub(int)", () -> ms(b.getMethod("pub", int.class)));
        rowx("getMethod pub(int,String)", () -> ms(b.getMethod("pub", int.class, String.class)));
        rowx("getMethod pub null params", () -> ms(b.getMethod("pub", (Class<?>[]) null)));
        rowx("getMethod pub(long) missing", () -> ms(b.getMethod("pub", long.class)));
        rowx("getMethod prot", () -> ms(b.getMethod("prot")));
        rowx("getMethod pkg", () -> ms(b.getMethod("pkg")));
        rowx("getMethod priv", () -> ms(b.getMethod("priv")));
        rowx("getMethod stat", () -> ms(b.getMethod("stat", Object.class)));
        rowx("getMethod derived stat inherited", () -> ms(d.getMethod("stat", Object.class)));
        rowx("getMethod derived pub inherited", () -> ms(d.getMethod("pub", int.class)));
        rowx("getMethod covariant", () -> ms(d.getMethod("cov")));
        rowx("getMethod iface default", () -> ms(d.getMethod("dflt")));
        rowx("getMethod iface abstract", () -> ms(d.getMethod("abs")));
        rowx("getMethod iface of iface", () -> ms(Marker.class.getMethod("abs")));
        rowx("getMethod Object methods on iface", () -> ms(Marker.class.getMethod("toString")));
        rowx("getMethod hashCode on array", () -> ms(int[].class.getMethod("hashCode")));
        rowx("getMethod clone on array", () -> ms(int[].class.getMethod("clone")));
        rowx("getMethod getClass on primitive", () -> ms(int.class.getMethod("getClass")));
        rowx("getMethod null name", () -> ms(b.getMethod(null)));
        rowx("getMethod <init>", () -> ms(b.getMethod("<init>")));
        rowx("getMethod <clinit>", () -> ms(b.getMethod("<clinit>")));
        rowx("getMethod varargs", () -> ms(b.getMethod("varargs", String[].class)));
        rowx("getMethod prims", () -> ms(b.getMethod("prims", int.class, long.class, double.class, boolean.class, byte.class,
                char.class, short.class, float.class)));
        rowx("getMethod arrs", () -> ms(b.getMethod("arrs", int[].class, String[][].class)));
        rowx("getMethod arrs wrong", () -> ms(b.getMethod("arrs", int[].class, String[].class)));
        rowx("getMethod null element", () -> ms(b.getMethod("pub", new Class<?>[] {null})));
        rowx("getMethod enum values", () -> ms(Color.class.getMethod("values")));
        rowx("getMethod enum valueOf", () -> ms(Color.class.getMethod("valueOf", String.class)));
        rowx("getMethod enum body values", () -> ms(Color.GREEN.getClass().getMethod("toString")));
        rowx("getMethod record accessor", () -> ms(Rec.class.getMethod("a")));
        rowx("getMethod String.valueOf(char[])", () -> ms(String.class.getMethod("valueOf", char[].class)));
        rowx("getMethod String.valueOf(Object)", () -> ms(String.class.getMethod("valueOf", Object.class)));
        rowx("getMethod Object.wait(long)", () -> ms(Object.class.getMethod("wait", long.class)));
        rowx("getMethod ArrayList.add(E)", () -> ms(java.util.ArrayList.class.getMethod("add", Object.class)));
        rowx("getMethod List.of", () -> ms(java.util.List.class.getMethod("of", Object.class)));
        rowx("getMethod ConcurrentHashMap.get", () -> ms(java.util.concurrent.ConcurrentHashMap.class.getMethod("get", Object.class)));
        rowx("getMethod HashMap non-public class method", () -> ms(java.util.HashMap.class.getMethod("keySet")));
        rowx("getMethod identity", () -> b.getMethod("pub") == b.getMethod("pub"));
        rowx("getMethod equals", () -> b.getMethod("pub").equals(b.getMethod("pub")));
        rowx("getMethod declaring/return", () -> {
            Method m = d.getMethod("pub", int.class);
            return m.getDeclaringClass().getName() + " " + m.getReturnType() + " " + m.getModifiers() + " " + m.getParameterCount();
        });
        rowx("getMethod invoke", () -> d.getMethod("cov").invoke(new Derived()));
        rowx("getMethod accessible leak", () -> {
            Method m1 = b.getMethod("pub");
            m1.setAccessible(false);
            Method m2 = b.getMethod("pub");
            return m1 == m2 ? "same" : "fresh " + m2.trySetAccessible();
        });
        rowx("getMethod bridge", () -> {
            StringBuilder sb = new StringBuilder();
            for (Method m : Comparable.class.getMethods()) sb.append(m).append(';');
            for (Method m : Integer.class.getMethods()) if (m.getName().equals("compareTo")) sb.append(m.isBridge()).append(';');
            return sb;
        });
        // ------------------------------------------------ getField
        rowx("getField pubF", () -> b.getField("pubF"));
        rowx("getField protF", () -> b.getField("protF"));
        rowx("getField pkgF", () -> b.getField("pkgF"));
        rowx("getField privF", () -> b.getField("privF"));
        rowx("getField STATIC_F", () -> b.getField("STATIC_F"));
        rowx("getField CONST get", () -> b.getField("CONST").get(null));
        rowx("getField inherited", () -> d.getField("pubF"));
        rowx("getField inherited iface const", () -> d.getField("MARK"));
        rowx("getField pkg-iface const via Derived", () -> d.getField("P"));
        rowx("getField derived own", () -> d.getField("derivedF"));
        rowx("getField missing", () -> d.getField("nope"));
        rowx("getField null", () -> d.getField(null));
        rowx("getField empty", () -> d.getField(""));
        rowx("getField length on array", () -> int[].class.getField("length"));
        rowx("getField on primitive", () -> int.class.getField("MAX_VALUE"));
        rowx("getField Integer.MAX_VALUE", () -> Integer.class.getField("MAX_VALUE").get(null));
        rowx("getField System.out", () -> System.class.getField("out").getType());
        rowx("getField identity", () -> b.getField("pubF") == b.getField("pubF"));
        rowx("getField equals", () -> b.getField("pubF").equals(b.getField("pubF")));
        rowx("getField get/set", () -> {
            Derived o = new Derived();
            Field f = d.getField("pubF");
            f.setInt(o, f.getInt(o) + 10);
            return o.pubF;
        });
        rowx("getField accessible leak", () -> {
            Field f1 = b.getField("pubF");
            f1.setAccessible(true);
            Field f2 = b.getField("pubF");
            return f1 == f2 ? "same" : "fresh";
        });
        rowx("getField Inner outer field", () -> Inner.class.getField("f"));
        rowx("getField this$0", () -> Inner.class.getField("this$0"));
        rowx("getField enum constant", () -> Color.class.getField("RED"));
        rowx("getField enum $VALUES", () -> Color.class.getField("$VALUES"));
        rowx("getFields", () -> all(d.getFields()));
        // ------------------------------------------------ getDeclaredField(s)
        rowx("getDeclaredField privF", () -> b.getDeclaredField("privF"));
        rowx("getDeclaredField pkgF", () -> b.getDeclaredField("pkgF"));
        rowx("getDeclaredField inherited", () -> d.getDeclaredField("pubF"));
        rowx("getDeclaredField after getField inherited", () -> {
            try {
                d.getField("pubF");
            } catch (Throwable t) {
                // ignore
            }
            return d.getDeclaredField("pubF");
        });
        rowx("getDeclaredField missing", () -> b.getDeclaredField("nope"));
        rowx("getDeclaredField null", () -> b.getDeclaredField(null));
        rowx("getDeclaredField this$0", () -> Inner.class.getDeclaredField("this$0"));
        rowx("getDeclaredField $VALUES", () -> Color.class.getDeclaredField("$VALUES"));
        rowx("getDeclaredField record", () -> Rec.class.getDeclaredField("a"));
        rowx("getDeclaredField on array", () -> int[].class.getDeclaredField("length"));
        rowx("getDeclaredField on primitive", () -> int.class.getDeclaredField("x"));
        rowx("getDeclaredField String.value", () -> String.class.getDeclaredField("value").getType());
        rowx("getDeclaredField Class.classLoader (filtered)", () -> Class.class.getDeclaredField("classLoader"));
        rowx("getDeclaredField Class.name", () -> Class.class.getDeclaredField("name"));
        rowx("getDeclaredField Field.clazz (filtered)", () -> Field.class.getDeclaredField("clazz"));
        rowx("getDeclaredField System.security", () -> System.class.getDeclaredField("security"));
        rowx("getDeclaredField identity", () -> b.getDeclaredField("privF") == b.getDeclaredField("privF"));
        rowx("getDeclaredField accessible", () -> {
            Field f = b.getDeclaredField("privF");
            f.setAccessible(true);
            Base o = new Base();
            f.setInt(o, 44);
            return f.getInt(o) + " " + b.getDeclaredField("privF").trySetAccessible();
        });
        for (Class<?> c : new Class<?>[] {b, d, Inner.class, Color.class, Rec.class, Empty.class, Marker.class, int.class, int[].class,
            String.class, Class.class, Field.class, Method.class, Object.class, Integer.class, System.class, Thread.class,
            java.util.ArrayList.class, Anno2.class, Color.GREEN.getClass()}) {
            final Class<?> cc = c;
            rowx("getDeclaredFields " + c.getName(), () -> {
                Field[] fs = cc.getDeclaredFields();
                StringBuilder sb = new StringBuilder(fs.length + ":");
                for (Field f : fs) sb.append(f.getName()).append(f.getModifiers()).append(',');
                return sb;
            });
        }
        rowx("getDeclaredFields fresh array", () -> b.getDeclaredFields() != b.getDeclaredFields());
        rowx("getDeclaredFields elements fresh", () -> b.getDeclaredFields()[0] == b.getDeclaredFields()[0]);
        rowx("getDeclaredFields equal", () -> Arrays.equals(b.getDeclaredFields(), b.getDeclaredFields()));
        rowx("getDeclaredFields accessible leak", () -> {
            Field a = b.getDeclaredFields()[3];
            a.setAccessible(true);
            return b.getDeclaredFields()[3].trySetAccessible() + " " + a.canAccess(new Base());
        });
        // ------------------------------------------------ getConstructor / getDeclaredConstructor
        rowx("getConstructor()", () -> b.getConstructor());
        rowx("getConstructor(int)", () -> b.getConstructor(int.class));
        rowx("getConstructor(String) protected", () -> b.getConstructor(String.class));
        rowx("getConstructor(long) private", () -> b.getConstructor(long.class));
        rowx("getConstructor null params", () -> b.getConstructor((Class<?>[]) null));
        rowx("getConstructor missing", () -> b.getConstructor(double.class));
        rowx("getConstructor derived inherited", () -> d.getConstructor(int.class));
        rowx("getConstructor abstract", () -> Abs.class.getConstructor());
        rowx("getConstructor interface", () -> Marker.class.getConstructor());
        rowx("getConstructor array", () -> int[].class.getConstructor());
        rowx("getConstructor primitive", () -> int.class.getConstructor());
        rowx("getConstructor enum", () -> Color.class.getConstructor());
        rowx("getConstructor record", () -> Rec.class.getConstructor(int.class, String.class));
        rowx("getConstructor inner", () -> Inner.class.getConstructor(SbrW6Classrefl.class));
        rowx("getConstructor inner no outer", () -> Inner.class.getConstructor());
        rowx("getConstructor String(char[])", () -> String.class.getConstructor(char[].class));
        rowx("getConstructor identity", () -> b.getConstructor() == b.getConstructor());
        rowx("getConstructor newInstance", () -> b.getConstructor(int.class).newInstance(3).getClass().getSimpleName());
        rowx("getConstructor equals", () -> b.getConstructor().equals(b.getConstructor()));
        rowx("getDeclaredConstructor()", () -> b.getDeclaredConstructor());
        rowx("getDeclaredConstructor(long) private", () -> b.getDeclaredConstructor(long.class));
        rowx("getDeclaredConstructor(String)", () -> b.getDeclaredConstructor(String.class));
        rowx("getDeclaredConstructor null params", () -> b.getDeclaredConstructor((Class<?>[]) null));
        rowx("getDeclaredConstructor missing", () -> b.getDeclaredConstructor(byte.class));
        rowx("getDeclaredConstructor derived inherited", () -> d.getDeclaredConstructor(int.class));
        rowx("getDeclaredConstructor private nested", () -> PrivNested.class.getDeclaredConstructor());
        rowx("getDeclaredConstructor enum", () -> Color.class.getDeclaredConstructor(String.class, int.class));
        rowx("getDeclaredConstructor enum body", () -> all(Color.GREEN.getClass().getDeclaredConstructors()));
        rowx("getDeclaredConstructor record", () -> Rec.class.getDeclaredConstructor(int.class, String.class));
        rowx("getDeclaredConstructor inner", () -> Inner.class.getDeclaredConstructor(SbrW6Classrefl.class));
        rowx("getDeclaredConstructor interface", () -> Marker.class.getDeclaredConstructor());
        rowx("getDeclaredConstructor array", () -> int[].class.getDeclaredConstructor());
        rowx("getDeclaredConstructor primitive", () -> void.class.getDeclaredConstructor());
        rowx("getDeclaredConstructor Object", () -> Object.class.getDeclaredConstructor());
        rowx("getDeclaredConstructor Class private", () -> Class.class.getDeclaredConstructor(ClassLoader.class, Class.class));
        rowx("getDeclaredConstructor accessible", () -> {
            Constructor<?> c = b.getDeclaredConstructor(long.class);
            c.setAccessible(true);
            return c.newInstance(1L).getClass().getSimpleName() + " " + b.getDeclaredConstructor(long.class).trySetAccessible();
        });
        rowx("getDeclaredConstructor identity", () -> b.getDeclaredConstructor() == b.getDeclaredConstructor());
        rowx("getDeclaredConstructor enum newInstance", () -> {
            Constructor<Color> c = Color.class.getDeclaredConstructor(String.class, int.class);
            c.setAccessible(true);
            return c.newInstance("X", 9);
        });
        rowx("getConstructors", () -> b.getConstructors().length + " " + d.getConstructors().length);
        // ------------------------------------------------ newInstance (held row; still behavioural)
        rowx("newInstance public", () -> Derived.class.newInstance().getClass().getSimpleName());
        rowx("newInstance private nested", () -> PrivNested.class.newInstance());
        rowx("newInstance abstract", () -> Abs.class.newInstance());
        rowx("newInstance interface", () -> Marker.class.newInstance());
        rowx("newInstance no default", () -> NoDefault.class.newInstance());
        rowx("newInstance thrower", () -> Thrower.class.newInstance());
        rowx("newInstance uthrower", () -> UThrower.class.newInstance());
        rowx("newInstance Class", () -> Class.class.newInstance());
        rowx("newInstance primitive", () -> int.class.newInstance());
        rowx("newInstance array", () -> int[].class.newInstance());
        rowx("newInstance enum", () -> Color.class.newInstance());
        rowx("newInstance inner", () -> Inner.class.newInstance());
        rowx("newInstance twice", () -> Derived.class.newInstance() != Derived.class.newInstance());
        rowx("newInstance Object", () -> Object.class.newInstance().getClass());
        rowx("newInstance ArrayList", () -> java.util.ArrayList.class.newInstance());
        // ------------------------------------------------ annotations
        for (Class<?> c : new Class<?>[] {b, d, Anno2.class, Color.class, Rec.class, Marker.class, int.class, int[].class, String.class,
            Object.class, Override.class, Deprecated.class, Inh.class, Rep.class, Reps.class, Runnable.class, Thread.class,
            java.util.concurrent.ConcurrentHashMap.class, FunctionalInterface.class, Documented.class}) {
            final Class<?> cc = c;
            rowx("annotations " + c.getName(), () -> anns(cc.getAnnotations()));
            rowx("declaredAnnotations " + c.getName(), () -> anns(cc.getDeclaredAnnotations()));
        }
        rowx("getAnnotation Inh on base", () -> b.getAnnotation(Inh.class));
        rowx("getAnnotation Inh on derived (inherited)", () -> d.getAnnotation(Inh.class));
        rowx("getDeclaredAnnotation Inh on derived", () -> d.getDeclaredAnnotation(Inh.class));
        rowx("getAnnotation Plain on base", () -> an(b.getAnnotation(Plain.class)));
        rowx("getAnnotation Plain on derived", () -> an(d.getAnnotation(Plain.class)));
        rowx("getAnnotation Rep container", () -> b.getAnnotation(Reps.class));
        rowx("getAnnotation Rep repeated (null)", () -> b.getAnnotation(Rep.class));
        rowx("getAnnotationsByType Rep", () -> anns(b.getAnnotationsByType(Rep.class)));
        rowx("getDeclaredAnnotationsByType Rep", () -> anns(b.getDeclaredAnnotationsByType(Rep.class)));
        rowx("getAnnotationsByType Plain on derived", () -> anns(d.getAnnotationsByType(Plain.class)));
        rowx("getAnnotationsByType Inh on derived", () -> anns(d.getAnnotationsByType(Inh.class)));
        rowx("getAnnotation Cls (class retention)", () -> b.getAnnotation(Cls.class));
        rowx("getAnnotation Src (source retention)", () -> b.getAnnotation(Src.class));
        rowx("getAnnotation null", () -> b.getAnnotation(null));
        rowx("getDeclaredAnnotation null", () -> b.getDeclaredAnnotation(null));
        rowx("getAnnotationsByType null", () -> b.getAnnotationsByType(null));
        rowx("isAnnotationPresent Inh", () -> b.isAnnotationPresent(Inh.class) + " " + d.isAnnotationPresent(Inh.class));
        rowx("isAnnotationPresent Plain", () -> b.isAnnotationPresent(Plain.class) + " " + d.isAnnotationPresent(Plain.class));
        rowx("isAnnotationPresent Rep/Reps", () -> b.isAnnotationPresent(Rep.class) + " " + b.isAnnotationPresent(Reps.class));
        rowx("isAnnotationPresent Cls", () -> b.isAnnotationPresent(Cls.class));
        rowx("isAnnotationPresent null", () -> b.isAnnotationPresent(null));
        rowx("isAnnotationPresent on primitive/array", () -> int.class.isAnnotationPresent(Plain.class) + " " + int[].class.isAnnotationPresent(Plain.class));
        rowx("isAnnotationPresent FunctionalInterface", () -> Runnable.class.isAnnotationPresent(FunctionalInterface.class));
        rowx("annotation identity cached", () -> b.getAnnotation(Plain.class) == b.getAnnotation(Plain.class));
        rowx("annotation identity across getters", () -> b.getAnnotation(Plain.class) == b.getDeclaredAnnotation(Plain.class));
        rowx("annotation identity arrays", () -> b.getAnnotations() == b.getAnnotations());
        rowx("annotation identity array elems", () -> b.getAnnotations()[0] == b.getAnnotations()[0]);
        rowx("annotation values", () -> {
            Plain p = b.getAnnotation(Plain.class);
            return p.n() + " " + p.k() + " " + Arrays.toString(p.s()) + " " + p.u() + " " + p.annotationType().getName();
        });
        rowx("annotation defaults", () -> {
            Plain p = d.getAnnotation(Plain.class);
            return p.n() + " " + p.k() + " " + Arrays.toString(p.s()) + " " + p.u();
        });
        rowx("annotation equals/hashCode", () -> {
            Plain p = b.getAnnotation(Plain.class);
            Plain q = d.getAnnotation(Plain.class);
            return p.equals(q) + " " + p.equals(p) + " " + (q.hashCode() == d.getAnnotation(Plain.class).hashCode());
        });
        rowx("annotation handler", () -> Proxy.getInvocationHandler(b.getAnnotation(Plain.class)).getClass().getName());
        rowx("annotation proxy class", () -> mask(b.getAnnotation(Plain.class).getClass().getName()) + " " + Proxy.isProxyClass(b.getAnnotation(Plain.class).getClass()));
        rowx("annotation type annotations", () -> anns(Plain.class.getAnnotations()) + " " + anns(Inh.class.getAnnotations()));
        rowx("annotation on annotated type", () -> Anno2.class.isAnnotation() + " " + anns(Anno2.class.getAnnotations()));
        rowx("annotation annotatedSuperclass", () -> b.getAnnotatedSuperclass().getType() + " " + Arrays.toString(b.getAnnotatedInterfaces()));
        rowx("annotation enum constant class", () -> anns(Color.GREEN.getClass().getAnnotations()));
        rowx("annotation Deprecated/FunctionalInterface", () -> anns(Runnable.class.getAnnotations()));
        rowx("annotation Documented on Deprecated", () -> Deprecated.class.getAnnotation(Documented.class) + " " + Deprecated.class.getAnnotation(Retention.class));
        rowx("annotation class order", () -> {
            Annotation[] a = b.getAnnotations();
            StringBuilder sb = new StringBuilder();
            for (Annotation x : a) sb.append(x.annotationType().getSimpleName()).append(',');
            return sb;
        });
        // ------------------------------------------------ enum constants
        rowx("getEnumConstants Color", () -> Arrays.toString(Color.class.getEnumConstants()));
        rowx("getEnumConstants fresh", () -> Color.class.getEnumConstants() != Color.class.getEnumConstants());
        rowx("getEnumConstants non-enum", () -> Base.class.getEnumConstants());
        rowx("getEnumConstants body class", () -> Color.GREEN.getClass().getEnumConstants());
        rowx("getEnumConstants Empty", () -> Empty.class.getEnumConstants().length);
        rowx("getEnumConstants TimeUnit", () -> Arrays.toString(TimeUnit.class.getEnumConstants()));
        rowx("getEnumConstants Thread.State", () -> Arrays.toString(Thread.State.class.getEnumConstants()));
        rowx("getEnumConstants primitive/array/iface", () -> int.class.getEnumConstants() + " " + int[].class.getEnumConstants() + " "
                + Runnable.class.getEnumConstants() + " " + Enum.class.getEnumConstants());
        rowx("Enum.valueOf", () -> Enum.valueOf(Color.class, "GREEN") + " " + Color.valueOf("BLUE"));
        rowx("Enum.valueOf missing", () -> Color.valueOf("PINK"));
        rowx("Enum.valueOf null name", () -> Enum.valueOf(Color.class, null));
        rowx("Enum.valueOf wrong class", () -> Enum.valueOf((Class) Base.class, "x"));
        rowx("Enum.valueOf Empty", () -> Empty.valueOf("A"));
        rowx("EnumSet.allOf", () -> EnumSet.allOf(Color.class) + " " + EnumSet.noneOf(TimeUnit.class) + " " + EnumSet.range(TimeUnit.SECONDS, TimeUnit.HOURS));
        rowx("EnumSet.allOf non-enum", () -> EnumSet.allOf((Class) Base.class));
        rowx("EnumMap", () -> {
            EnumMap<Color, Integer> m = new EnumMap<>(Color.class);
            m.put(Color.BLUE, 1);
            m.put(Color.RED, 2);
            return m + " " + m.size();
        });
        rowx("EnumMap non-enum", () -> new EnumMap((Class) Base.class));
        rowx("enum switch", () -> {
            StringBuilder sb = new StringBuilder();
            for (TimeUnit u : TimeUnit.values()) {
                switch (u) {
                    case SECONDS -> sb.append('s');
                    case DAYS -> sb.append('d');
                    default -> sb.append('.');
                }
            }
            return sb;
        });
        // ------------------------------------------------ misc surface
        rowx("getDeclaredMethods count", () -> b.getDeclaredMethods().length + " " + d.getDeclaredMethods().length);
        rowx("getMethods count", () -> b.getMethods().length + " " + d.getMethods().length);
        rowx("getEnclosing", () -> Inner.class.getEnclosingClass() + " " + local().getEnclosingMethod() + " " + anon().getEnclosingMethod());
        rowx("getDeclaringClass", () -> Inner.class.getDeclaringClass() + " " + local().getDeclaringClass() + " " + int.class.getDeclaringClass());
        rowx("isMemberClass etc", () -> Inner.class.isMemberClass() + " " + local().isLocalClass() + " " + anon().isAnonymousClass() + " "
                + Base.class.isMemberClass() + " " + Object.class.isMemberClass());
        rowx("toString/toGenericString", () -> b + " | " + Marker.class + " | " + int.class + " | " + int[].class + " | " + Color.class.toGenericString()
                + " | " + java.util.Map.class.toGenericString());
        rowx("nest", () -> Inner.class.getNestHost() + " " + Inner.class.isNestmateOf(Base.class));
        rowx("cast/asSubclass", () -> b.cast(new Derived()).getClass() + " " + d.asSubclass(b));
        rowx("getInterfaces", () -> Arrays.toString(d.getInterfaces()) + Arrays.toString(int[].class.getInterfaces()));
        rowx("getSuperclass", () -> d.getSuperclass() + " " + Marker.class.getSuperclass() + " " + int[].class.getSuperclass());
        rowx("proxy reflection", () -> {
            Class<?> p = proxy();
            return p.getMethod("abs") + " " + p.getMethods().length + " " + p.getDeclaredFields().length + " "
                    + p.getConstructor(java.lang.reflect.InvocationHandler.class);
        });
        rowx("lambda reflection", () -> {
            Class<?> l = lambdaClass();
            return l.getDeclaredFields().length + " " + l.getDeclaredMethods().length + " " + mask(String.valueOf(l.getDeclaredConstructors().length));
        });
        rowx("getMethod on lambda", () -> lambdaClass().getMethod("run"));
        rowx("reflect via getMethod invoke chain", () -> {
            Object o = new Derived();
            Method m = o.getClass().getMethod("pub", int.class, String.class);
            m.invoke(o, 1, "a");
            return m.getName() + m.getParameterTypes().length;
        });
        rowx("getMethod on core-filtered", () -> ms(Class.class.getMethod("getName")));
        rowx("getDeclaredField on Reflection", () -> Class.forName("jdk.internal.reflect.Reflection").getDeclaredFields().length);
        rowx("getDeclaredMethods on Class count nonzero", () -> Class.class.getDeclaredMethods().length > 100);

        // ------------------------------------------------ evaluation
        String[] first = new String[ROWS.size()];
        for (int i = 0; i < ROWS.size(); i++) {
            first[i] = mask(safe(ROWS.get(i)));
            System.out.println("row " + LABELS.get(i) + " = " + units(first[i]));
        }
        int drift = 0;
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 200;
        for (int k = 0; k < iters; k++) {
            for (int i = 0; i < ROWS.size(); i++) {
                String v = mask(safe(ROWS.get(i)));
                if (!v.equals(first[i])) {
                    if (drift < 10) {
                        System.out.println("DRIFT at iter " + k + " " + LABELS.get(i) + " = " + units(v));
                    }
                    drift++;
                }
            }
        }
        System.out.println("rows " + ROWS.size() + " drift " + drift);
        // The reflection and annotation caches hang off soft references and side stores:
        // collect, churn, and ask again.
        int gcdrift = 0;
        for (int g = 0; g < 4; g++) {
            byte[][] junk = new byte[64][];
            for (int j = 0; j < junk.length; j++) junk[j] = new byte[1 << 20];
            System.gc();
            for (int i = 0; i < ROWS.size(); i++) {
                String v = mask(safe(ROWS.get(i)));
                if (!v.equals(first[i])) {
                    if (gcdrift < 10) {
                        System.out.println("GCDRIFT at round " + g + " " + LABELS.get(i) + " = " + units(v));
                    }
                    gcdrift++;
                }
            }
        }
        System.out.println("rows " + ROWS.size() + " gcdrift " + gcdrift);
    }
}
