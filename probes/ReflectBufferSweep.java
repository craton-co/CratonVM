import java.lang.reflect.*;
import java.nio.*;
import java.util.*;

/** java.lang.Class / java.lang.reflect.Field / java.nio.ByteBuffer / Thread,
 *  the next four families off the bridge-kind retirement surface
 *  (63 + 31 + 31 + 33 rows), diffed against HotSpot.
 *
 *  Deterministic only: no thread scheduling, no identity hash codes, no
 *  addresses, no wall-clock. Every value is hex-escaped so the diff cannot
 *  depend on either VM's stdout encoding. */
public class ReflectBufferSweep {
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
    interface ThrowingRun { void run() throws Exception; }

    // ---- fixtures the reflection half inspects --------------------------
    interface Marker {}
    static class Base implements Marker { protected int bi; public String bs; }
    static final class Sub extends Base {
        private static final long SERIAL = 7L;
        private int i = 3; private String s = "x"; private double d = 1.5;
        private final int[] arr = {1, 2};
        Sub() {}
        Sub(int i) { this.i = i; }
        private int twice(int n) { return n * 2; }
        public String toString() { return "Sub(" + i + "," + s + ")"; }
    }
    enum Color { RED, GREEN }

    static void classFamily() {
        Class<?>[] cs = { Sub.class, Base.class, Marker.class, int.class, void.class,
                          int[].class, String[][].class, Color.class, Map.Entry.class,
                          Object.class, String.class };
        for (Class<?> c : cs) {
            String k = "[" + c.getName() + "]";
            p(k + " getSimpleName", c.getSimpleName());
            p(k + " getCanonicalName", c.getCanonicalName());
            p(k + " isInterface", c.isInterface());
            p(k + " isArray", c.isArray());
            p(k + " isPrimitive", c.isPrimitive());
            p(k + " isEnum", c.isEnum());
            p(k + " isAnonymousClass", c.isAnonymousClass());
            p(k + " isMemberClass", c.isMemberClass());
            p(k + " isSynthetic", c.isSynthetic());
            p(k + " getModifiers", Modifier.toString(c.getModifiers()));
            p(k + " getSuperclass", c.getSuperclass());
            p(k + " interfaces", Arrays.toString(c.getInterfaces()));
            p(k + " getComponentType", c.getComponentType());
            p(k + " isAssignableFrom(Sub)", c.isAssignableFrom(Sub.class));
            p(k + " Sub.isAssignableFrom(c)", Sub.class.isAssignableFrom(c));
            p(k + " isInstance(\"s\")", c.isInstance("s"));
            p(k + " arrayType", c == void.class ? "n/a" : c.arrayType().getName());
            p(k + " descriptorString", c.descriptorString());
            p(k + " toString", c.toString());
        }
        p("Sub declaredFields", sortedNames(Sub.class.getDeclaredFields()));
        p("Sub fields(public)", sortedNames(Sub.class.getFields()));
        p("Base fields(public)", sortedNames(Base.class.getFields()));
        p("Sub declaredMethods", sortedNames(Sub.class.getDeclaredMethods()));
        p("Sub declaredCtors count", Sub.class.getDeclaredConstructors().length);
        p("Color enumConstants", Arrays.toString(Color.class.getEnumConstants()));
        t("Sub.getField(i) is not public", () -> Sub.class.getField("i"));
        t("Sub.getDeclaredField(nope)", () -> Sub.class.getDeclaredField("nope"));
        t("Sub.getDeclaredMethod(twice)", () -> Sub.class.getDeclaredMethod("twice", int.class));
    }
    static String sortedNames(Member[] ms) {
        List<String> l = new ArrayList<>();
        for (Member m : ms) l.add(m.getName());
        Collections.sort(l);
        return l.toString();
    }

    static void fieldFamily() throws Exception {
        Sub o = new Sub(11);
        for (String n : new String[]{"i", "s", "d", "arr", "SERIAL"}) {
            Field f = Sub.class.getDeclaredField(n);
            f.setAccessible(true);
            String k = "[Field " + n + "]";
            p(k + " getType", f.getType().getName());
            p(k + " getModifiers", Modifier.toString(f.getModifiers()));
            p(k + " getGenericType", f.getGenericType().getTypeName());
            p(k + " isSynthetic", f.isSynthetic());
            p(k + " getDeclaringClass", f.getDeclaringClass().getSimpleName());
            p(k + " toString", f.toString());
            if (!Modifier.isStatic(f.getModifiers())) {
                Object v = f.get(o);
                p(k + " get", v instanceof int[] ? Arrays.toString((int[]) v) : v);
            }
        }
        Field fi = Sub.class.getDeclaredField("i"); fi.setAccessible(true);
        fi.setInt(o, 42);
        p("Field setInt then get", fi.getInt(o));
        p("Field object after set", o.toString());
        t("Field setInt on String field", () -> {
            Field fs = Sub.class.getDeclaredField("s"); fs.setAccessible(true); fs.setInt(o, 1); });
        t("Field get wrong receiver", () -> fi.get("not-a-Sub"));
        Method m = Sub.class.getDeclaredMethod("twice", int.class); m.setAccessible(true);
        p("Method invoke", m.invoke(o, 21));
        p("Method getReturnType", m.getReturnType().getName());
        p("Method paramTypes", Arrays.toString(m.getParameterTypes()));
        p("Method paramCount", m.getParameterCount());
        p("Method toString", m.toString());
        t("Method invoke wrong arg type", () -> m.invoke(o, "s"));
        t("Method invoke wrong arity", () -> m.invoke(o));
        Constructor<Sub> ctor = Sub.class.getDeclaredConstructor(int.class);
        ctor.setAccessible(true);
        p("Constructor newInstance", ctor.newInstance(5));
        p("Array.newInstance", Arrays.toString((int[]) Array.newInstance(int.class, 3)));
        int[] ia = {7, 8};
        p("Array.get", Array.get(ia, 1));
        p("Array.getLength", Array.getLength(ia));
        t("Array.get oob", () -> Array.get(ia, 9));
    }

    static void byteBufferFamily() {
        for (int cap : new int[]{16}) {
            ByteBuffer b = ByteBuffer.allocate(cap);
            p("bb order default", b.order());
            b.putInt(0x01020304).putShort((short) 0x0506).put((byte) 7);
            p("bb position", b.position());
            p("bb remaining", b.remaining());
            p("bb limit/capacity", b.limit() + "/" + b.capacity());
            b.flip();
            p("bb after flip pos/limit", b.position() + "/" + b.limit());
            p("bb getInt", Integer.toHexString(b.getInt()));
            p("bb getShort", Integer.toHexString(b.getShort()));
            p("bb get", b.get());
            b.rewind();
            p("bb LITTLE_ENDIAN getInt",
              Integer.toHexString(b.order(ByteOrder.LITTLE_ENDIAN).getInt()));
            b.order(ByteOrder.BIG_ENDIAN).rewind();
            p("bb hasArray", b.hasArray());
            p("bb arrayOffset", b.arrayOffset());
            p("bb isDirect", b.isDirect());
            p("bb isReadOnly", b.isReadOnly());
            byte[] dst = new byte[4];
            b.get(dst);
            p("bb bulk get", Arrays.toString(dst));
            p("bb slice remaining", b.slice().remaining());
            p("bb duplicate pos", b.duplicate().position());
            p("bb asReadOnlyBuffer isReadOnly", b.asReadOnlyBuffer().isReadOnly());
            p("bb compact then pos", b.compact().position());
            b.clear();
            p("bb after clear pos/limit", b.position() + "/" + b.limit());
            p("bb equals self-duplicate", b.equals(b.duplicate()));
            p("bb toString", b.toString());
            ByteBuffer w = ByteBuffer.wrap(new byte[]{1, 2, 3, 4});
            p("bb wrap getInt", w.getInt());
            p("bb asIntBuffer cap", ByteBuffer.allocate(8).asIntBuffer().capacity());
            p("bb asLongBuffer cap", ByteBuffer.allocate(16).asLongBuffer().capacity());
            t("bb getInt underflow", () -> ByteBuffer.allocate(2).getInt());
            t("bb put on read-only", () -> ByteBuffer.allocate(4).asReadOnlyBuffer().put((byte) 1));
            t("bb negative allocate", () -> ByteBuffer.allocate(-1));
            t("bb position beyond limit", () -> ByteBuffer.allocate(4).position(9));
        }
    }

    static void threadFamily() throws Exception {
        Thread t = new Thread(() -> {}, "probe-thread");
        p("thread getName", t.getName());
        p("thread isAlive before start", t.isAlive());
        p("thread isDaemon default", t.isDaemon());
        p("thread getState NEW", t.getState());
        t.setDaemon(true);
        p("thread isDaemon after set", t.isDaemon());
        t.setPriority(Thread.MIN_PRIORITY);
        p("thread getPriority", t.getPriority());
        p("thread getThreadGroup name", t.getThreadGroup() == null ? "null" : t.getThreadGroup().getName());
        t.setName("renamed");
        p("thread renamed", t.getName());
        p("current isAlive", Thread.currentThread().isAlive());
        p("current getState", Thread.currentThread().getState());
        p("MIN/NORM/MAX", Thread.MIN_PRIORITY + "/" + Thread.NORM_PRIORITY + "/" + Thread.MAX_PRIORITY);
        p("interrupted flag clean", Thread.interrupted());
        Thread.currentThread().interrupt();
        p("interrupted after set", Thread.interrupted());
        p("interrupted cleared by read", Thread.interrupted());
        t.start(); t.join();
        p("thread getState after join", t.getState());
        p("thread isAlive after join", t.isAlive());
        t("start twice", () -> t.start());
        t("setPriority out of range", () -> new Thread(() -> {}).setPriority(99));
        t("null name", () -> new Thread(() -> {}).setName(null));
    }

    public static void main(String[] a) throws Exception {
        classFamily();
        fieldFamily();
        byteBufferFamily();
        threadFamily();
        System.out.println("DONE ReflectBufferSweep");
    }
}
