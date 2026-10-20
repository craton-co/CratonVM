import java.io.InputStream;
import java.io.ObjectStreamClass;
import java.io.ObjectStreamField;
import java.io.Serializable;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Array;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.function.Supplier;

// CLASSSHAPE19-1 census: `Class.getComponentType()`, `componentType()`, `descriptorString()`
// (and what is built on them: `getTypeName`, `arrayType`, `MethodType` descriptors,
// `ObjectStreamField` signatures) over a wide set of mirrors, against HotSpot 25. Every line is
// deterministic; the timing rows at the end are `<row> <N> ns/op`.
// Usage: R19ClassComponentCensus [timing]
public class R19ClassComponentCensus {
    static final class OwnLoader extends ClassLoader {
        OwnLoader(ClassLoader parent) { super(parent); }
        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (!name.startsWith("R19CccOwn")) return super.loadClass(name, resolve);
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    try (InputStream in = R19ClassComponentCensus.class.getClassLoader().getResourceAsStream(name + ".class")) {
                        byte[] b = in.readAllBytes();
                        c = defineClass(name, b, 0, b.length);
                    } catch (Exception e) {
                        throw new ClassNotFoundException(name, e);
                    }
                }
                return c;
            }
        }
    }
    interface Iface { }
    enum Color { RED }
    @interface Ann { }
    record Point(int x) { }
    static class Inner { }
    static final class Ser implements Serializable {
        int[] ints; String[][] names; Inner[] inners; long l; Object o;
    }

    static String n(Class<?> c) { return c == null ? "null" : c.getName(); }

    static void row(String label, Class<?> c, Class<?> expectedComponent) {
        Class<?> gct = c.getComponentType();
        Class<?> ct = c.componentType();
        System.out.println("comp " + label + " name=" + n(c) + " getComponentType=" + n(gct) + " componentType()=" + n(ct)
            + " same=" + (gct == ct) + " expected=" + (gct == expectedComponent)
            + " desc=" + c.descriptorString() + " type=" + c.getTypeName()
            + (c.isPrimitive() && c == void.class ? "" : " arrayComp=" + (c.arrayType().getComponentType() == c))
            + " elemLoader=" + loaderOf(c));
    }
    static String loaderOf(Class<?> c) {
        while (c.isArray()) c = c.getComponentType();
        ClassLoader l = c.getClassLoader();
        return l == null ? "boot" : l.getClass().getSimpleName();
    }

    public static void main(String[] a) throws Throwable {
        row("int", int.class, null);
        row("void", void.class, null);
        row("Object", Object.class, null);
        row("Iface", Iface.class, null);
        row("int[]", int[].class, int.class);
        row("int[][]", int[][].class, int[].class);
        row("boolean[]", boolean[].class, boolean.class);
        row("String[]", String[].class, String.class);
        row("String[][]", String[][].class, String[].class);
        row("Object[]", Object[].class, Object.class);
        row("List[]", List[].class, List.class);
        row("Map.Entry[]", Map.Entry[].class, Map.Entry.class);
        row("Inner[]", Inner[].class, Inner.class);
        row("Color[]", Color[].class, Color.class);
        row("Ann[]", Ann[].class, Ann.class);
        row("Point[][][]", Point[][][].class, Point[][].class);
        class Local { }
        row("Local[]", Local[].class, Local.class);
        Object anon = new Object() { };
        row("anon[]", Array.newInstance(anon.getClass(), 0).getClass(), anon.getClass());
        Supplier<String> lam = () -> "x";
        Class<?> lamArr = Array.newInstance(lam.getClass(), 1).getClass();
        System.out.println("lambda-array isArray=" + lamArr.isArray() + " comp=" + (lamArr.getComponentType() == lam.getClass())
            + " ct=" + (lamArr.componentType() == lam.getClass()) + " arrayType=" + (lam.getClass().arrayType() == lamArr)
            + " descStartsWith=" + lamArr.descriptorString().startsWith("[LR19ClassComponentCensus$$Lambda"));
        MethodHandles.Lookup lk = MethodHandles.lookup();
        byte[] hb;
        try (InputStream in = R19ClassComponentCensus.class.getResourceAsStream("R19CccHidden.class")) { hb = in.readAllBytes(); }
        Class<?> hidden = lk.defineHiddenClass(hb, false).lookupClass();
        Class<?> hArr = hidden.arrayType();
        System.out.println("hidden-array isArray=" + hArr.isArray() + " comp=" + (hArr.getComponentType() == hidden)
            + " ct=" + (hArr.componentType() == hidden) + " desc=" + hArr.descriptorString().replaceAll("0x[0-9a-f]+", "0x#")
            + " type=" + hArr.getTypeName().replaceAll("0x[0-9a-f]+", "0x#"));
        Class<?> forNamed = Class.forName("[LR19CccInit;", false, R19ClassComponentCensus.class.getClassLoader());
        System.out.println("forname-array comp=" + n(forNamed.getComponentType()) + " ct=" + n(forNamed.componentType()) + " desc=" + forNamed.descriptorString()
            + " initialized=" + R19CccInitFlag.initialized);
        Class<?> own = new OwnLoader(R19ClassComponentCensus.class.getClassLoader()).loadClass("R19CccOwn");
        row("own[] (getClass)", Array.newInstance(own, 2).getClass(), own);
        row("own[] (arrayType)", own.arrayType(), own);
        Method one = own.getMethod("one");
        row("own.one()", one.getReturnType(), own);
        row("own.two()", own.getMethod("two").getReturnType(), own.arrayType());
        row("own.items", own.getDeclaredField("items").getType(), own);
        System.out.println("own-identity " + (one.getReturnType() == own.arrayType()) + " "
            + (own.getMethod("two").getReturnType().getComponentType().getComponentType() == own) + " "
            + (own.arrayType() != R19CccOwn.class.arrayType()));
        System.out.println("methodtype " + MethodType.methodType(int[].class, String[][].class, Inner[].class).descriptorString()
            + " " + MethodType.methodType(void.class, own.arrayType()).toMethodDescriptorString());
        for (ObjectStreamField f : ObjectStreamClass.lookup(Ser.class).getFields()) {
            System.out.println("osf " + f.getName() + " " + f.getTypeCode() + " " + f.getTypeString() + " " + n(f.getType()));
        }
        List<Class<?>> chain = new ArrayList<>();
        for (Class<?> c = int[][][].class; c != null; c = c.getComponentType()) chain.add(c);
        System.out.println("chain " + chain.size() + " " + n(chain.get(chain.size() - 1)));
        System.out.println("varhandle " + MethodHandles.arrayElementVarHandle(String[].class).varType().getName()
            + " " + MethodHandles.arrayElementGetter(Inner[].class).type());
        if (a.length > 0) timing();
    }

    static volatile Object sink;
    static void timing() {
        Class<?>[] cs = { int[].class, String[].class, Object.class, Inner[][].class, List.class, long[].class, Color[].class, int.class };
        for (String rowName : new String[] { "get-component-type", "component-type", "descriptor-string", "type-name" }) {
            long[] best = new long[9];
            for (int rep = 0; rep < 9; rep++) {
                int iters = 200_000;
                long t0 = System.nanoTime();
                Object o = null;
                for (int i = 0; i < iters; i++) {
                    Class<?> c = cs[i & 7];
                    switch (rowName) {
                        case "get-component-type": o = c.getComponentType(); break;
                        case "component-type": o = c.componentType(); break;
                        case "descriptor-string": o = c.descriptorString(); break;
                        default: o = c.getTypeName(); break;
                    }
                }
                sink = o;
                best[rep] = (System.nanoTime() - t0) / iters;
            }
            java.util.Arrays.sort(best);
            System.out.println(rowName + " " + best[4] + " ns/op");
        }
    }
}

class R19CccOwn {
    R19CccOwn[] items;
    public R19CccOwn[] one() { return null; }
    public R19CccOwn[][] two() { return null; }
}
class R19CccInitFlag { static boolean initialized; }
class R19CccInit { static { R19CccInitFlag.initialized = true; } }
class R19CccHidden { }
