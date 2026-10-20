// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;
import java.lang.reflect.AnnotatedType;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.GenericArrayType;
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
import java.lang.reflect.ParameterizedType;
import java.lang.reflect.Proxy;
import java.lang.reflect.Type;
import java.lang.reflect.TypeVariable;
import java.lang.reflect.WildcardType;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Map;
import java.util.function.Supplier;

/**
 * SBR wave 3, lane {@code reflect}: the core-reflection family --
 * {@code java.lang.reflect.Field/Method/Constructor/Proxy}, the generic
 * {@code sun.reflect.generics.reflectiveObjects.*Impl} carriers, and
 * {@code jdk.internal.reflect.ReflectionFactory}'s copy door (reached through
 * every {@code Class.getMethod/getField/getConstructor}).
 *
 * <p>Every row is evaluated ONCE and printed (the first-run value -- a native
 * that wins from the first call shows here), then re-evaluated inside a hot
 * loop and compared with that first value; {@code drift} counts mismatches, so
 * cold == warm is asserted rather than assumed. Diff the whole output against
 * HotSpot 25. Nothing printed depends on identity hashes or on a proxy
 * counter (digits in proxy class names are masked).
 *
 * <pre>
 *   java SbrW3Reflect                       (HotSpot oracle)
 *   cratonvm --jdk-only -cp . SbrW3Reflect
 * </pre>
 */
public class SbrW3Reflect {
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
                .replaceAll("@[0-9a-f]{4,}", "@H");
    }

    // ---------------------------------------------------------------- fixtures

    @Retention(RetentionPolicy.RUNTIME)
    @Target({ElementType.FIELD, ElementType.METHOD, ElementType.CONSTRUCTOR, ElementType.PARAMETER,
        ElementType.TYPE_PARAMETER, ElementType.TYPE})
    @interface Tag {
        String value();

        int n() default 3;
    }

    public static class Holder<T extends Number> {
        public int i = 7;
        public long l = 1L << 40;
        public short s = -3;
        public byte b = 9;
        public char c = 'q';
        public boolean z = true;
        public float f = 1.5f;
        public double d = 2.25;
        public String str = "hello";
        public final int fin = 11;
        public static int si = 5;
        public static String ss = "static";
        private int priv = 42;
        @Tag("fld") public Map<String, List<T>> generic;
        public T tvar;
        public T[] tarr;
        public List<? extends Number> wild;
        public List<? super Integer> wildLo;
        public Map.Entry<String, Integer> nested;

        public Holder() {}

        @Tag("ctor")
        private Holder(@Tag("p0") int x) {
            this.i = x;
        }

        @Tag("m")
        public int add(int a, @Tag("p1") int b) {
            return a + b + i;
        }

        public static String cat(String a, Object b) {
            return a + ":" + b;
        }

        public String varargs(String... xs) {
            return String.join("|", xs);
        }

        public void boom() {
            throw new IllegalStateException("boom");
        }

        private String hidden() {
            return "hidden";
        }

        public <U extends Comparable<U>> List<U> gen(Map<String, ? extends U> m, U[] arr) {
            return null;
        }
    }

    @Retention(RetentionPolicy.RUNTIME)
    @Target({ElementType.TYPE_USE, ElementType.TYPE_PARAMETER})
    @interface TagU {
        String value() default "u";
    }

    /** A type parameter that CARRIES annotations, so the TypeVariable annotation rows are not default-bound. */
    public static class Ann<@Tag("tp") @TagU("tpu") T extends @TagU("bound") Number> {
        public <@TagU("mtp") M extends @TagU("mb") Comparable<M>> M m(M x) {
            return x;
        }
    }

    public static class Outer<A> {
        public class Inner<B> {}

        public Outer<String>.Inner<Integer> field;
    }

    public interface Greeter {
        String greet(String who);

        int num();

        default String dflt(String x) {
            return "dflt-" + x;
        }
    }

    interface Hidden {
        String h();
    }

    static final InvocationHandler H = (p, m, a) -> {
        switch (m.getName()) {
            case "greet":
                return "hi " + a[0];
            case "num":
                return 17;
            case "hashCode":
                return 1234;
            case "equals":
                return p == a[0];
            case "toString":
                return "proxy!";
            case "h":
                return "hid";
            case "dflt":
                return InvocationHandler.invokeDefault(p, m, a);
            default:
                throw new UnsupportedOperationException(m.getName());
        }
    };

    static String types(Type[] ts) {
        StringBuilder b = new StringBuilder("[");
        for (Type t : ts) b.append(describe(t)).append(';');
        return b.append(']').toString();
    }

    static String describe(Type t) {
        if (t == null) return "null";
        String k;
        if (t instanceof Class) k = "C";
        else if (t instanceof ParameterizedType) k = "P";
        else if (t instanceof TypeVariable) k = "V";
        else if (t instanceof WildcardType) k = "W";
        else if (t instanceof GenericArrayType) k = "A";
        else k = "?";
        return k + ":" + t.getTypeName();
    }

    public static void main(String[] args) throws Exception {
        Holder<Integer> h = new Holder<>();
        Class<?> hc = Holder.class;

        // ------------------------------------------------ Field get / set
        for (String n : new String[] {"i", "l", "s", "b", "c", "z", "f", "d", "str", "fin", "si", "ss"}) {
            rowx("field.get " + n, () -> {
                Field f = hc.getField(n);
                return f.get(Modifier.isStatic(f.getModifiers()) ? null : h);
            });
        }
        rowx("field.getInt i", () -> hc.getField("i").getInt(h));
        rowx("field.getLong i (widen)", () -> hc.getField("i").getLong(h));
        rowx("field.getLong l", () -> hc.getField("l").getLong(h));
        rowx("field.getDouble f (widen)", () -> hc.getField("f").getDouble(h));
        rowx("field.getFloat f", () -> hc.getField("f").getFloat(h));
        rowx("field.getDouble d", () -> hc.getField("d").getDouble(h));
        rowx("field.getShort s", () -> hc.getField("s").getShort(h));
        rowx("field.getByte b", () -> hc.getField("b").getByte(h));
        rowx("field.getChar c", () -> hc.getField("c").getChar(h));
        rowx("field.getBoolean z", () -> hc.getField("z").getBoolean(h));
        rowx("field.getInt c (widen char)", () -> hc.getField("c").getInt(h));
        rowx("field.getInt l (narrow: IAE)", () -> hc.getField("l").getInt(h));
        rowx("field.getInt str (IAE)", () -> hc.getField("str").getInt(h));
        rowx("field.get wrong receiver", () -> hc.getField("i").get("notaholder"));
        rowx("field.get null receiver", () -> hc.getField("i").get(null));
        rowx("field.set/get round trip i", () -> {
            Holder<Integer> x = new Holder<>();
            Field f = hc.getField("i");
            f.set(x, 99);
            return f.get(x) + "/" + x.i;
        });
        rowx("field.setInt/getInt i", () -> {
            Holder<Integer> x = new Holder<>();
            hc.getField("i").setInt(x, -5);
            return x.i;
        });
        rowx("field.setLong l", () -> {
            Holder<Integer> x = new Holder<>();
            hc.getField("l").setLong(x, 77L);
            return x.l;
        });
        rowx("field.setLong i (narrow IAE)", () -> {
            Holder<Integer> x = new Holder<>();
            hc.getField("i").setLong(x, 77L);
            return x.i;
        });
        rowx("field.setInt l (widen)", () -> {
            Holder<Integer> x = new Holder<>();
            hc.getField("l").setInt(x, 3);
            return x.l;
        });
        rowx("field.setShort s", () -> {
            Holder<Integer> x = new Holder<>();
            hc.getField("s").setShort(x, (short) 300);
            return x.s;
        });
        rowx("field.setByte b", () -> {
            Holder<Integer> x = new Holder<>();
            hc.getField("b").setByte(x, (byte) -1);
            return x.b;
        });
        rowx("field.setChar c", () -> {
            Holder<Integer> x = new Holder<>();
            hc.getField("c").setChar(x, 'Z');
            return x.c;
        });
        rowx("field.setBoolean z false", () -> {
            Holder<Integer> x = new Holder<>();
            hc.getField("z").setBoolean(x, false);
            return x.z;
        });
        rowx("field.setFloat f", () -> {
            Holder<Integer> x = new Holder<>();
            hc.getField("f").setFloat(x, 8.5f);
            return x.f;
        });
        rowx("field.setDouble d", () -> {
            Holder<Integer> x = new Holder<>();
            hc.getField("d").setDouble(x, -0.5);
            return x.d;
        });
        rowx("field.set str wrong type (IAE)", () -> {
            Holder<Integer> x = new Holder<>();
            hc.getField("str").set(x, 12);
            return x.str;
        });
        rowx("field.set i null (IAE)", () -> {
            Holder<Integer> x = new Holder<>();
            hc.getField("i").set(x, null);
            return x.i;
        });
        rowx("field.set final (IAE)", () -> {
            Holder<Integer> x = new Holder<>();
            hc.getField("fin").setInt(x, 1);
            return x.fin;
        });
        rowx("field.set final accessible", () -> {
            Holder<Integer> x = new Holder<>();
            Field f = hc.getField("fin");
            f.setAccessible(true);
            f.setInt(x, 12);
            return f.getInt(x);
        });
        rowx("field.set static si", () -> {
            Field f = hc.getField("si");
            int old = Holder.si;
            f.setInt(null, 6);
            int now = Holder.si;
            Holder.si = old;
            return now;
        });
        rowx("field.get private (IAE)", () -> hc.getDeclaredField("priv").get(h));
        rowx("field.get private accessible", () -> {
            Field f = hc.getDeclaredField("priv");
            f.setAccessible(true);
            return f.get(h);
        });
        rowx("field.canAccess private", () -> hc.getDeclaredField("priv").canAccess(h));
        rowx("field.canAccess public", () -> hc.getField("i").canAccess(h));
        rowx("field.canAccess static with receiver (IAE)", () -> hc.getField("si").canAccess(h));
        rowx("field.trySetAccessible jdk-internal", () -> String.class.getDeclaredField("value").trySetAccessible());
        rowx("field.setAccessible jdk-internal", () -> {
            String.class.getDeclaredField("hash").setAccessible(true);
            return "set";
        });
        rowx("field copy model: same object", () -> hc.getField("i") == hc.getField("i"));
        rowx("field copy model: equals/hash", () -> {
            Field a = hc.getField("i"), b = hc.getField("i");
            return a.equals(b) + " " + (a.hashCode() == b.hashCode());
        });
        rowx("field accessible does not leak", () -> {
            Field a = hc.getDeclaredField("priv");
            a.setAccessible(true);
            Field b = hc.getDeclaredField("priv");
            return a.canAccess(h) + " " + b.canAccess(h);
        });
        rowx("field.toString", () -> hc.getDeclaredField("priv").toString());
        rowx("field.toGenericString", () -> hc.getField("generic").toGenericString());
        rowx("field.getAnnotation", () -> hc.getField("generic").getAnnotation(Tag.class));
        rowx("field.getDeclaredAnnotations", () -> Arrays.toString(hc.getField("generic").getDeclaredAnnotations()));
        rowx("field.getAnnotatedType", () -> {
            AnnotatedType at = hc.getField("generic").getAnnotatedType();
            return describe(at.getType());
        });
        rowx("field.getDeclaredFields names", () -> {
            StringBuilder b = new StringBuilder();
            for (Field f : Outer.class.getDeclaredFields()) b.append(f.getName()).append(',');
            return b;
        });

        // ------------------------------------------------ generics
        rowx("gen field generic", () -> describe(hc.getField("generic").getGenericType()));
        rowx("gen PT args", () -> types(((ParameterizedType) hc.getField("generic").getGenericType()).getActualTypeArguments()));
        rowx("gen PT raw", () -> describe(((ParameterizedType) hc.getField("generic").getGenericType()).getRawType()));
        rowx("gen PT owner", () -> describe(((ParameterizedType) hc.getField("generic").getGenericType()).getOwnerType()));
        rowx("gen PT toString", () -> hc.getField("generic").getGenericType().toString());
        rowx("gen PT args clone", () -> {
            ParameterizedType p = (ParameterizedType) hc.getField("generic").getGenericType();
            Type[] a = p.getActualTypeArguments();
            a[0] = null;
            return describe(p.getActualTypeArguments()[0]) + " " + a.getClass().getComponentType().getName();
        });
        rowx("gen PT equals/hash", () -> {
            Type a = hc.getField("generic").getGenericType(), b = hc.getField("generic").getGenericType();
            return a.equals(b) + " " + (a.hashCode() == b.hashCode());
        });
        rowx("gen PT nested entry", () -> describe(hc.getField("nested").getGenericType()) + " owner="
                + describe(((ParameterizedType) hc.getField("nested").getGenericType()).getOwnerType()));
        rowx("gen PT inner owner", () -> {
            ParameterizedType p = (ParameterizedType) Outer.class.getField("field").getGenericType();
            return describe(p) + " owner=" + describe(p.getOwnerType()) + " raw=" + describe(p.getRawType())
                    + " args=" + types(p.getActualTypeArguments());
        });
        rowx("gen TV field", () -> describe(hc.getField("tvar").getGenericType()));
        rowx("gen TV name/bounds", () -> {
            TypeVariable<?> v = (TypeVariable<?>) hc.getField("tvar").getGenericType();
            return v.getName() + " " + types(v.getBounds()) + " decl=" + v.getGenericDeclaration();
        });
        rowx("gen TV toString/typeName", () -> {
            Type v = hc.getField("tvar").getGenericType();
            return v.toString() + " " + v.getTypeName();
        });
        rowx("gen TV annotations", () -> {
            TypeVariable<?> v = (TypeVariable<?>) hc.getField("tvar").getGenericType();
            return v.getAnnotations().length + " " + v.getDeclaredAnnotations().length + " "
                    + v.getAnnotatedBounds().length + " " + v.getAnnotation(Tag.class);
        });
        rowx("gen TV equals", () -> hc.getField("tvar").getGenericType().equals(hc.getTypeParameters()[0]));
        rowx("gen class type params", () -> types(hc.getTypeParameters()));
        rowx("gen TV annotated: getAnnotations", () -> Arrays.toString(Ann.class.getTypeParameters()[0].getAnnotations()));
        rowx("gen TV annotated: getDeclaredAnnotations", () -> Arrays.toString(Ann.class.getTypeParameters()[0].getDeclaredAnnotations()));
        rowx("gen TV annotated: getAnnotation", () -> Ann.class.getTypeParameters()[0].getAnnotation(Tag.class));
        rowx("gen TV annotated: getDeclaredAnnotation", () -> Ann.class.getTypeParameters()[0].getDeclaredAnnotation(TagU.class));
        rowx("gen TV annotated: byType", () -> Ann.class.getTypeParameters()[0].getAnnotationsByType(Tag.class).length + " "
                + Ann.class.getTypeParameters()[0].getDeclaredAnnotationsByType(TagU.class).length);
        rowx("gen TV annotated: bounds", () -> {
            AnnotatedType[] b = Ann.class.getTypeParameters()[0].getAnnotatedBounds();
            return b.length + " " + describe(b[0].getType()) + " " + Arrays.toString(b[0].getAnnotations());
        });
        rowx("gen TV method annotated", () -> {
            TypeVariable<Method> v = Ann.class.getMethod("m", Comparable.class).getTypeParameters()[0];
            return Arrays.toString(v.getAnnotations()) + " " + Arrays.toString(v.getAnnotatedBounds()[0].getAnnotations());
        });
        rowx("gen GA field", () -> {
            Type t = hc.getField("tarr").getGenericType();
            return describe(t) + " comp=" + describe(((GenericArrayType) t).getGenericComponentType());
        });
        rowx("gen W upper", () -> {
            ParameterizedType p = (ParameterizedType) hc.getField("wild").getGenericType();
            WildcardType w = (WildcardType) p.getActualTypeArguments()[0];
            return describe(w) + " up=" + types(w.getUpperBounds()) + " lo=" + types(w.getLowerBounds());
        });
        rowx("gen W lower", () -> {
            ParameterizedType p = (ParameterizedType) hc.getField("wildLo").getGenericType();
            WildcardType w = (WildcardType) p.getActualTypeArguments()[0];
            return describe(w) + " up=" + types(w.getUpperBounds()) + " lo=" + types(w.getLowerBounds());
        });
        rowx("gen method return", () -> describe(hc.getMethod("gen", Map.class, Comparable[].class).getGenericReturnType()));
        rowx("gen method params", () -> types(hc.getMethod("gen", Map.class, Comparable[].class).getGenericParameterTypes()));
        rowx("gen method type params", () -> {
            TypeVariable<Method>[] tv = hc.getMethod("gen", Map.class, Comparable[].class).getTypeParameters();
            return types(tv) + " bounds=" + types(tv[0].getBounds());
        });
        rowx("gen method toGenericString", () -> hc.getMethod("gen", Map.class, Comparable[].class).toGenericString());
        rowx("gen superclass", () -> describe(java.util.ArrayList.class.getGenericSuperclass()));
        rowx("gen interfaces", () -> types(java.util.HashMap.class.getGenericInterfaces()));

        // ------------------------------------------------ Method
        rowx("method.invoke instance", () -> hc.getMethod("add", int.class, int.class).invoke(h, 1, 2));
        rowx("method.invoke static", () -> hc.getMethod("cat", String.class, Object.class).invoke(null, "a", 5));
        rowx("method.invoke varargs", () -> hc.getMethod("varargs", String[].class).invoke(h, (Object) new String[] {"x", "y"}));
        rowx("method.invoke throws", () -> hc.getMethod("boom").invoke(h));
        rowx("method.invoke wrong arity", () -> hc.getMethod("add", int.class, int.class).invoke(h, 1));
        rowx("method.invoke wrong type", () -> hc.getMethod("add", int.class, int.class).invoke(h, "x", 2));
        rowx("method.invoke null receiver", () -> hc.getMethod("add", int.class, int.class).invoke(null, 1, 2));
        rowx("method.invoke wrong receiver", () -> hc.getMethod("add", int.class, int.class).invoke("s", 1, 2));
        rowx("method.invoke private (IAE)", () -> hc.getDeclaredMethod("hidden").invoke(h));
        rowx("method.invoke private accessible", () -> {
            Method m = hc.getDeclaredMethod("hidden");
            m.setAccessible(true);
            return m.invoke(h);
        });
        rowx("method.invoke String.length", () -> String.class.getMethod("length").invoke("abcd"));
        rowx("method.invoke Integer.valueOf", () -> Integer.class.getMethod("valueOf", int.class).invoke(null, 41));
        rowx("method.invoke interface default", () -> Greeter.class.getMethod("dflt", String.class).invoke(
                (Greeter) new Greeter() {
                    public String greet(String w) { return w; }
                    public int num() { return 0; }
                }, "z"));
        rowx("method.canAccess private", () -> hc.getDeclaredMethod("hidden").canAccess(h));
        rowx("method.canAccess public", () -> hc.getMethod("add", int.class, int.class).canAccess(h));
        rowx("method.canAccess static null", () -> hc.getMethod("cat", String.class, Object.class).canAccess(null));
        rowx("method copy model", () -> {
            Method a = hc.getMethod("add", int.class, int.class), b = hc.getMethod("add", int.class, int.class);
            return (a == b) + " " + a.equals(b) + " " + (a.hashCode() == b.hashCode());
        });
        rowx("method.getAnnotation", () -> hc.getMethod("add", int.class, int.class).getAnnotation(Tag.class));
        rowx("method.getAnnotations", () -> Arrays.toString(hc.getMethod("add", int.class, int.class).getAnnotations()));
        rowx("method.getParameterAnnotations", () -> Arrays.deepToString(hc.getMethod("add", int.class, int.class).getParameterAnnotations()));
        rowx("method.getGenericReturnType", () -> describe(hc.getMethod("add", int.class, int.class).getGenericReturnType()));
        rowx("method.getGenericParameterTypes", () -> types(hc.getMethod("add", int.class, int.class).getGenericParameterTypes()));
        rowx("method.getDefaultValue", () -> Tag.class.getMethod("n").getDefaultValue());
        rowx("method.toString", () -> hc.getMethod("add", int.class, int.class).toString());
        rowx("method.getParameters", () -> Arrays.toString(hc.getMethod("add", int.class, int.class).getParameters()));

        // ------------------------------------------------ Constructor
        rowx("ctor.newInstance public", () -> ((Holder<?>) hc.getConstructor().newInstance()).i);
        rowx("ctor.newInstance private nestmate", () -> ((Holder<?>) hc.getDeclaredConstructor(int.class).newInstance(3)).i);
        rowx("ctor.newInstance jdk private (IAE)", () -> Class.forName("java.lang.invoke.MethodHandles$Lookup")
                .getDeclaredConstructor(Class.class).newInstance(String.class));
        rowx("ctor.newInstance private accessible", () -> {
            Constructor<?> c = hc.getDeclaredConstructor(int.class);
            c.setAccessible(true);
            return ((Holder<?>) c.newInstance(3)).i;
        });
        rowx("ctor.newInstance wrong arity", () -> hc.getConstructor().newInstance(1));
        rowx("ctor.newInstance StringBuilder", () -> StringBuilder.class.getConstructor(String.class).newInstance("sb").reverse());
        rowx("ctor.newInstance abstract", () -> Number.class.getConstructor().newInstance());
        rowx("ctor.canAccess private", () -> hc.getDeclaredConstructor(int.class).canAccess(null));
        rowx("ctor.setAccessible jdk-internal", () -> {
            Constructor<?> c = Class.forName("java.lang.invoke.MethodHandles$Lookup").getDeclaredConstructor(Class.class);
            c.setAccessible(true);
            return "set";
        });
        rowx("ctor copy model", () -> {
            Constructor<?> a = hc.getConstructor(), b = hc.getConstructor();
            return (a == b) + " " + a.equals(b) + " " + (a.hashCode() == b.hashCode());
        });
        rowx("ctor.getAnnotations", () -> Arrays.toString(hc.getDeclaredConstructor(int.class).getAnnotations()));
        rowx("ctor.getParameterAnnotations", () -> Arrays.deepToString(hc.getDeclaredConstructor(int.class).getParameterAnnotations()));
        rowx("ctor.toString", () -> hc.getDeclaredConstructor(int.class).toString());

        // ------------------------------------------------ Proxy
        rowx("proxy.new+call", () -> {
            Greeter g = (Greeter) Proxy.newProxyInstance(Greeter.class.getClassLoader(), new Class<?>[] {Greeter.class}, H);
            return g.greet("bob") + " " + g.num() + " " + g.hashCode() + " " + g.toString() + " " + g.equals(g);
        });
        rowx("proxy.default method", () -> {
            Greeter g = (Greeter) Proxy.newProxyInstance(Greeter.class.getClassLoader(), new Class<?>[] {Greeter.class}, H);
            return g.dflt("q");
        });
        rowx("proxy.isProxyClass proxy", () -> Proxy.isProxyClass(
                Proxy.newProxyInstance(Greeter.class.getClassLoader(), new Class<?>[] {Greeter.class}, H).getClass()));
        rowx("proxy.isProxyClass Object", () -> Proxy.isProxyClass(Object.class));
        rowx("proxy.isProxyClass Proxy", () -> Proxy.isProxyClass(Proxy.class));
        rowx("proxy.isProxyClass null", () -> Proxy.isProxyClass(null));
        rowx("proxy.getInvocationHandler", () -> Proxy.getInvocationHandler(
                Proxy.newProxyInstance(Greeter.class.getClassLoader(), new Class<?>[] {Greeter.class}, H)) == H);
        rowx("proxy.getInvocationHandler non-proxy", () -> Proxy.getInvocationHandler("x"));
        rowx("proxy.getInvocationHandler null", () -> Proxy.getInvocationHandler(null));
        rowx("proxy.class shape", () -> {
            Class<?> c = Proxy.newProxyInstance(Hidden.class.getClassLoader(), new Class<?>[] {Hidden.class}, H).getClass();
            return mask(c.getName()) + " super=" + c.getSuperclass().getName() + " ifs=" + Arrays.toString(c.getInterfaces())
                    + " final=" + Modifier.isFinal(c.getModifiers()) + " pkgSame=" + (c.getPackageName().equals(Hidden.class.getPackageName()));
        });
        rowx("proxy.hidden iface call", () -> ((Hidden) Proxy.newProxyInstance(Hidden.class.getClassLoader(),
                new Class<?>[] {Hidden.class}, H)).h());
        rowx("proxy.same class twice", () -> Proxy.newProxyInstance(Greeter.class.getClassLoader(), new Class<?>[] {Greeter.class}, H).getClass()
                == Proxy.newProxyInstance(Greeter.class.getClassLoader(), new Class<?>[] {Greeter.class}, H).getClass());
        rowx("proxy.getProxyClass", () -> {
            @SuppressWarnings("deprecation")
            Class<?> c = Proxy.getProxyClass(Greeter.class.getClassLoader(), Greeter.class);
            Greeter g = (Greeter) c.getConstructor(InvocationHandler.class).newInstance(H);
            return g.greet("pc") + " " + Proxy.isProxyClass(c);
        });
        rowx("proxy.null handler (NPE)", () -> Proxy.newProxyInstance(Greeter.class.getClassLoader(), new Class<?>[] {Greeter.class}, null));
        rowx("proxy.non-interface (IAE)", () -> Proxy.newProxyInstance(Greeter.class.getClassLoader(), new Class<?>[] {String.class}, H));
        rowx("proxy.undeclared checked", () -> {
            InvocationHandler bad = (p, m, a) -> {
                throw new Exception("checked");
            };
            return ((Greeter) Proxy.newProxyInstance(Greeter.class.getClassLoader(), new Class<?>[] {Greeter.class}, bad)).greet("x");
        });
        rowx("proxy.annotation", () -> {
            Tag t = hc.getMethod("add", int.class, int.class).getAnnotation(Tag.class);
            return t.value() + " " + t.n() + " " + t.annotationType().getName() + " " + Proxy.isProxyClass(t.getClass());
        });

        // ------------------------------------------------ misc
        rowx("record components", () -> {
            record P(int a, String b) {}
            StringBuilder b = new StringBuilder();
            for (var rc : P.class.getRecordComponents()) b.append(rc.getName()).append('=').append(rc.getAccessor().invoke(new P(1, "x"))).append(',');
            return b;
        });

        String[] first = new String[ROWS.size()];
        for (int i = 0; i < ROWS.size(); i++) {
            first[i] = mask(safe(ROWS.get(i)));
            System.out.println("row " + LABELS.get(i) + " = " + units(first[i]));
        }
        int drift = 0;
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 300;
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
    }
}
