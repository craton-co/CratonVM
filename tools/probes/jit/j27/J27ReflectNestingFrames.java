// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// j27 wave 5 (lane REFLECT): the `(Native Method)` frame of `Class.getDeclaringClass0`
// on top of the IncompatibleClassChangeError HotSpot's `Reflection::check_for_inner_class`
// raises inside it (page
// docs/known-issues/j27-spoofed-member-icce-omits-the-getdeclaringclass0-frame-20261028.md).
//
// `j27nest.G1` claims (its own InnerClasses row) to be a member of `j27nest.O1`, which
// has no InnerClasses attribute; a user-defined loader defines both. Every `Class` query
// that reaches `getDeclaringClass0` throws. `j27nest.O2$M` / `j27nest.O2` is a consistent
// pair (the control: no throw).
//
// One `fact <name> <value>` line per checked answer. A raise prints its class, its message,
// then its top two stack frames as `Class.method`, `(native)` appended for a native frame.
// Deterministic; no timing.
//
// Run (both modes, default flags):
//   javac -d out J27ReflectNestingFrames.java
//   java -cp out J27ReflectNestingFrames > hs.txt                  # HotSpot 25 = the reference
//   cratonvm -cp out J27ReflectNestingFrames | diff - hs.txt       # --real-jdk
//   cratonvm --compatible -cp out J27ReflectNestingFrames | diff - hs.txt
//
// Expected HotSpot 25 lines (frames read from the JDK 25 sources and HotSpot's
// `JVM_GetDeclaringClass`; take the HotSpot run as the reference):
//   fact nest-declaring-class java.lang.IncompatibleClassChangeError: j27nest.O1 and j27nest.G1 disagree on InnerClasses attribute @ java.lang.Class.getDeclaringClass0(native) | java.lang.Class.getDeclaringClass
//   fact nest-simple-name java.lang.IncompatibleClassChangeError: j27nest.O1 and j27nest.G1 disagree on InnerClasses attribute @ java.lang.Class.getDeclaringClass0(native) | java.lang.Class.isTopLevelClass
//   fact nest-enclosing-class-top java.lang.Class.getDeclaringClass0(native)
//   fact nest-is-member-top java.lang.Class.getDeclaringClass0(native)
//   fact nest-declaring-class-hot-throws 20000
//   fact nest-declaring-class-hot   (the same value as nest-declaring-class)
//   fact nest-control-simple-name M
//   fact nest-control-declaring-class j27nest.O2
// Before the j27 wave 5 fix CratonVM listed `java.lang.Class.getDeclaringClass` /
// `java.lang.Class.isTopLevelClass` first (no getDeclaringClass0 frame).
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.util.HashMap;
import java.util.Map;

public class J27ReflectNestingFrames {

    static final int HOT = 20_000;

    static void fact(String name, Object value) {
        System.out.println("fact " + name + " " + value);
    }

    static String frame(StackTraceElement e) {
        return e.getClassName() + "." + e.getMethodName() + (e.isNativeMethod() ? "(native)" : "");
    }

    /** Class, message and the top two frames. */
    static String describe(Throwable t) {
        StackTraceElement[] st = t.getStackTrace();
        StringBuilder sb = new StringBuilder(t.getClass().getName()).append(": ").append(t.getMessage());
        sb.append(" @");
        for (int i = 0; i < Math.min(2, st.length); i++) {
            sb.append(i == 0 ? " " : " | ").append(frame(st[i]));
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

    /** The top frame of what `body` throws, or `no-throw:<value>`. */
    static void topRow(String name, Body body) {
        Object v;
        try {
            v = "no-throw:" + body.run();
        } catch (Throwable t) {
            StackTraceElement[] st = t.getStackTrace();
            v = st.length == 0 ? "no-frames" : frame(st[0]);
        }
        fact(name, v);
    }

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
            super("j27nest", J27ReflectNestingFrames.class.getClassLoader());
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            byte[] b = bytes.get(name);
            if (b == null) throw new ClassNotFoundException(name);
            return defineClass(name, b, 0, b.length);
        }
    }

    public static void main(String[] args) throws Throwable {
        Loader l = new Loader();
        l.bytes.put("j27nest.O1", classBytes("j27nest/O1", null));
        l.bytes.put("j27nest.G1", classBytes("j27nest/G1", new String[] {"j27nest/G1", "j27nest/O1", "G1"}));
        String[] member = {"j27nest/O2$M", "j27nest/O2", "M"};
        l.bytes.put("j27nest.O2", classBytes("j27nest/O2", member));
        l.bytes.put("j27nest.O2$M", classBytes("j27nest/O2$M", member));
        Class<?> g1 = l.loadClass("j27nest.G1");

        row("nest-declaring-class", g1::getDeclaringClass);
        row("nest-simple-name", g1::getSimpleName);
        topRow("nest-enclosing-class-top", g1::getEnclosingClass);
        topRow("nest-is-member-top", g1::isMemberClass);

        int throwsSeen = 0;
        Throwable last = null;
        for (int i = 0; i < HOT; i++) {
            try {
                g1.getDeclaringClass();
            } catch (IncompatibleClassChangeError e) {
                throwsSeen++;
                last = e;
            }
        }
        fact("nest-declaring-class-hot-throws", throwsSeen);
        fact("nest-declaring-class-hot", last == null ? "no-throw" : describe(last));

        Class<?> m = l.loadClass("j27nest.O2$M");
        row("nest-control-simple-name", m::getSimpleName);
        row("nest-control-declaring-class", () -> m.getDeclaringClass().getName());
    }
}
