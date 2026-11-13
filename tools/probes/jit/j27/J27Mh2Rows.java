// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// j27 wave 4 (lane MHROWS2): the second batch of MethodHandle rows of
// docs/feature-designs/jit-ir-optimizations.md
// (WMTE-1, WMTE-2, MH16-5, MHAPP-3, MH717-2).
//
// One `fact <name> <value>` line per checked answer. A raise prints its class,
// its message, then its stack frames up to the first frame of this class: JDK
// frames as `Class.method`, the probe's frame with its line. Deterministic.
//
//   * wmte1-*    asType unbox / cast refusals of a REAL handle, cold and after
//                20 000 accepted calls on the same call site (the hot doors);
//   * wmte2-*    AsVarargsCollector.asTypeUncached's `cannot build collector`;
//   * mh16-5-*   bindTo(null) of a real handle (static and virtual members);
//   * mhapp3-*   MethodType identity (interning) of handles' types;
//   * mh717-2-*  an exact-type refusal whose message names a class whose
//                InnerClasses claim is not confirmed by its outer class.
//
// Run (both modes, default flags):
//   javac -d out J27Mh2Rows.java
//   java -cp out J27Mh2Rows > hs.txt                  # HotSpot 25 = the reference
//   cratonvm -cp out J27Mh2Rows | diff - hs.txt       # --real-jdk
//   cratonvm --compatible -cp out J27Mh2Rows | diff - hs.txt
//
// Expected HotSpot 25 lines (frames read from the JDK 25 sources; the probe
// lines are this file's -- take the HotSpot run as the reference):
//   fact wmte1-unconvertible-cold java.lang.ClassCastException: Cannot cast java.lang.Long to java.lang.Integer @ java.lang.Class.cast | sun.invoke.util.ValueConversions.primitiveConversion | sun.invoke.util.ValueConversions.unboxInteger | J27Mh2Rows.unboxRow:<n>
//   fact wmte1-unconvertible-hot   (the same line)
//   fact wmte1-null-cold java.lang.NullPointerException: Cannot invoke "java.lang.Number.intValue()" because the return value of "sun.invoke.util.ValueConversions.primitiveConversion(sun.invoke.util.Wrapper, Object, boolean)" is null @ sun.invoke.util.ValueConversions.unboxInteger | J27Mh2Rows.unboxRow:<n>
//   fact wmte1-not-a-number-cold java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.Number (java.lang.String and java.lang.Number are in module java.base of loader 'bootstrap') @ sun.invoke.util.ValueConversions.primitiveConversion | sun.invoke.util.ValueConversions.unboxInteger | J27Mh2Rows.unboxRow:<n>
//   fact wmte1-field-receiver-cold java.lang.ClassCastException: Cannot cast java.lang.String to J27Mh2Rows$Acc @ java.lang.Class.cast | J27Mh2Rows.fieldRow:<n>
//   fact wmte1-field-receiver-hot   (the same line)
//   fact wmte2-collector java.lang.invoke.WrongMethodTypeException: cannot build collector @ java.lang.invoke.MethodHandleImpl$AsVarargsCollector.asTypeUncached | java.lang.invoke.MethodHandle.asType | J27Mh2Rows.collectorRow:<n> cause=java.lang.IllegalArgumentException
//   fact mh16-5-bind-null-static echo:null
//   fact mh16-5-bind-null-static-hot echo:null
//   fact mh16-5-bind-null-virtual java.lang.NullPointerException
//   fact mhapp3-findStatic-type-interned true      (and every other mhapp3-* line: true)
//   fact mh717-2-wmte-spoofed java.lang.IncompatibleClassChangeError: j27mh2.O1 and j27mh2.G1 disagree on InnerClasses attribute
//   fact wmte1-not-a-number-hot   (the same line as -cold)
// The not-a-number rows need the `ServedRaiseHopSite::LocalCheckcast` hop
// (j27 wave 4 follow-up); before it CratonVM listed only the probe frame.
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.HashMap;
import java.util.Map;

public class J27Mh2Rows {
    static final class Acc {
        final int base;

        Acc(int base) {
            this.base = base;
        }
    }

    static int twiceI(int x) {
        return 2 * x;
    }

    static String echo(String s) {
        return "echo:" + s;
    }

    static String join(String head, String... rest) {
        return head + rest.length;
    }

    static final int HOT = 20_000;
    static final MethodHandles.Lookup L = MethodHandles.lookup();

    static void fact(String name, Object value) {
        System.out.println("fact " + name + " " + value);
    }

    static String describe(Throwable t) {
        StringBuilder sb = new StringBuilder(t.getClass().getName()).append(": ").append(t.getMessage());
        sb.append(" @");
        String sep = " ";
        for (StackTraceElement e : t.getStackTrace()) {
            sb.append(sep);
            if (e.getClassName().equals("J27Mh2Rows")) {
                sb.append("J27Mh2Rows.").append(e.getMethodName()).append(':').append(e.getLineNumber());
                break;
            }
            sb.append(e.getClassName()).append('.').append(e.getMethodName());
            sep = " | ";
        }
        if (t.getCause() != null) {
            sb.append(" cause=").append(t.getCause().getClass().getName());
        }
        return sb.toString();
    }

    interface Body {
        Object run() throws Throwable;
    }

    static void row(String name, Body body) {
        Object v;
        try {
            v = body.run();
        } catch (Throwable t) {
            v = describe(t);
        }
        fact(name, v);
    }

    // ---- WMTE-1: asType unbox / reference cast refusals --------------------

    /** One call of `(int)Object` on `h`: the raise's line is this method's. */
    static Object unboxRow(MethodHandle h, Object v) throws Throwable {
        return (int) h.invokeExact(v);
    }

    /** `invoke` of a field getter at `(Object)int`: the receiver's cast. */
    static Object fieldRow(MethodHandle getter, Object receiver) throws Throwable {
        return (int) getter.invoke(receiver);
    }

    static void wmte1() throws Throwable {
        MethodHandle twice = L.findStatic(J27Mh2Rows.class, "twiceI",
                MethodType.methodType(int.class, int.class));
        MethodHandle h = twice.asType(MethodType.methodType(int.class, Object.class));
        row("wmte1-unconvertible-cold", () -> unboxRow(h, Long.valueOf(3)));
        row("wmte1-null-cold", () -> unboxRow(h, null));
        row("wmte1-not-a-number-cold", () -> unboxRow(h, "s"));
        long sum = 0;
        for (int i = 0; i < HOT; i++) {
            sum += (int) unboxRow(h, Integer.valueOf(i & 7));
        }
        fact("wmte1-unbox-sum", sum);
        row("wmte1-unconvertible-hot", () -> unboxRow(h, Long.valueOf(3)));
        row("wmte1-null-hot", () -> unboxRow(h, null));
        row("wmte1-not-a-number-hot", () -> unboxRow(h, "s"));

        MethodHandle getter = L.findGetter(Acc.class, "base", int.class);
        Acc acc = new Acc(5);
        row("wmte1-field-receiver-cold", () -> fieldRow(getter, "x"));
        long fsum = 0;
        for (int i = 0; i < HOT; i++) {
            fsum += (int) fieldRow(getter, acc);
        }
        fact("wmte1-field-sum", fsum);
        row("wmte1-field-receiver-hot", () -> fieldRow(getter, "x"));
    }

    // ---- WMTE-2: AsVarargsCollector's `cannot build collector` -------------

    static Object collectorRow(MethodHandle collector) {
        return collector.asType(MethodType.methodType(String.class));
    }

    static void wmte2() throws Throwable {
        MethodHandle j = L.findStatic(J27Mh2Rows.class, "join",
                MethodType.methodType(String.class, String.class, String[].class));
        fact("wmte2-is-varargs", j.isVarargsCollector());
        row("wmte2-collector", () -> collectorRow(j));
        row("wmte2-collect-two", () -> (String) j.invoke("a", "b", "c"));
    }

    // ---- MH16-5: bindTo(null) of a real handle -----------------------------

    static void mh165() throws Throwable {
        MethodHandle echo = L.findStatic(J27Mh2Rows.class, "echo",
                MethodType.methodType(String.class, String.class));
        row("mh16-5-bind-null-static", () -> (String) echo.bindTo(null).invokeExact());
        String last = null;
        for (int i = 0; i < HOT; i++) {
            last = (String) echo.bindTo(null).invokeExact();
        }
        fact("mh16-5-bind-null-static-hot", last);
        fact("mh16-5-bind-null-type", echo.bindTo(null).type());
        MethodHandle len = L.findVirtual(String.class, "length", MethodType.methodType(int.class));
        Object v;
        try {
            v = (int) len.bindTo(null).invokeExact();
        } catch (Throwable t) {
            v = t.getClass().getName();
        }
        fact("mh16-5-bind-null-virtual", v);
    }

    // ---- MHAPP-3: MethodType interning --------------------------------------

    static void mhapp3() throws Throwable {
        MethodType ii = MethodType.methodType(int.class, int.class);
        fact("mhapp3-findStatic-type-interned",
                L.findStatic(J27Mh2Rows.class, "twiceI", ii).type() == ii);
        fact("mhapp3-identity-type-interned",
                MethodHandles.identity(String.class).type()
                        == MethodType.methodType(String.class, String.class));
        MethodType accInt = MethodType.methodType(Acc.class, int.class);
        MethodHandle ctor = L.findConstructor(Acc.class, MethodType.methodType(void.class, int.class));
        fact("mhapp3-findConstructor-type-interned", ctor.type() == accInt);
        MethodHandle unreflected = L.unreflectConstructor(Acc.class.getDeclaredConstructor(int.class));
        fact("mhapp3-unreflectConstructor-type-interned", unreflected.type() == accInt);
        fact("mhapp3-revealDirect-ctor-type-interned",
                L.revealDirect(ctor).getMethodType() == MethodType.methodType(void.class, int.class));
        MethodHandle bound = L.findStatic(J27Mh2Rows.class, "echo",
                MethodType.methodType(String.class, String.class)).bindTo("x");
        fact("mhapp3-bindTo-type-interned", bound.type() == MethodType.methodType(String.class));
        fact("mhapp3-asType-type-interned",
                L.findStatic(J27Mh2Rows.class, "twiceI", ii)
                        .asType(MethodType.methodType(Object.class, Object.class)).type()
                        == MethodType.methodType(Object.class, Object.class));
    }

    // ---- MH717-2: a spoofed member named by an exact-type refusal ----------

    /** A class file with no members, optionally one InnerClasses row (inner, outer, simple). */
    static byte[] classBytes(String name, String[] innerRow) throws IOException {
        Map<String, Integer> utf8 = new HashMap<>();
        Map<String, Integer> classes = new HashMap<>();
        ByteArrayOutputStream poolBytes = new ByteArrayOutputStream();
        DataOutputStream pool = new DataOutputStream(poolBytes);
        int[] count = {0};
        java.util.function.Function<String, Integer> u = s -> utf8.computeIfAbsent(s, k -> {
            try {
                pool.writeByte(1);
                pool.writeUTF(k);
            } catch (IOException e) {
                throw new RuntimeException(e);
            }
            return ++count[0];
        });
        java.util.function.Function<String, Integer> c = s -> {
            Integer have = classes.get(s);
            if (have != null) return have;
            int nameIndex = u.apply(s);
            try {
                pool.writeByte(7);
                pool.writeShort(nameIndex);
            } catch (IOException e) {
                throw new RuntimeException(e);
            }
            classes.put(s, ++count[0]);
            return count[0];
        };
        int thisIndex = c.apply(name);
        int superIndex = c.apply("java/lang/Object");
        int[] row = null;
        int attrName = 0;
        if (innerRow != null) {
            attrName = u.apply("InnerClasses");
            row = new int[] {c.apply(innerRow[0]), c.apply(innerRow[1]), u.apply(innerRow[2]), 0x0009};
        }
        pool.flush();
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        DataOutputStream d = new DataOutputStream(out);
        d.writeInt(0xCAFEBABE);
        d.writeShort(0);
        d.writeShort(61);
        d.writeShort(count[0] + 1);
        d.write(poolBytes.toByteArray());
        d.writeShort(0x0021);
        d.writeShort(thisIndex);
        d.writeShort(superIndex);
        d.writeShort(0);
        d.writeShort(0);
        d.writeShort(0);
        if (row == null) {
            d.writeShort(0);
        } else {
            d.writeShort(1);
            d.writeShort(attrName);
            d.writeInt(2 + 8);
            d.writeShort(1);
            for (int v : row) d.writeShort(v);
        }
        d.flush();
        return out.toByteArray();
    }

    static final class Loader extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();

        Loader() {
            super("j27mh2", J27Mh2Rows.class.getClassLoader());
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            byte[] b = bytes.get(name);
            if (b == null) throw new ClassNotFoundException(name);
            return defineClass(name, b, 0, b.length);
        }
    }

    static void mh7172() throws Throwable {
        Loader l = new Loader();
        // G1 claims to be a member of O1; O1 has no InnerClasses attribute.
        l.bytes.put("j27mh2.O1", classBytes("j27mh2/O1", null));
        l.bytes.put("j27mh2.G1", classBytes("j27mh2/G1", new String[] {"j27mh2/G1", "j27mh2/O1", "G1"}));
        Class<?> g1 = l.loadClass("j27mh2.G1");
        MethodHandle id = MethodHandles.identity(g1);
        row("mh717-2-wmte-spoofed", () -> (Object) id.invokeExact((Object) null));
        row("mh717-2-simple-name", g1::getSimpleName);
    }

    public static void main(String[] args) throws Throwable {
        wmte1();
        wmte2();
        mh165();
        mhapp3();
        mh7172();
    }
}
