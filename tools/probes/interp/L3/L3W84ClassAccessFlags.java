/*
 * Interpreter round i4, wave 84, lane L3: jdk.internal.reflect.Reflection
 * .getClassAccessFlags, the class-header flags the JDK's class-accessibility
 * checks read (VerifyAccess.getClassModifiers, Reflection.verifyMemberAccess).
 *
 * HotSpot's JVM_GetClassAccessFlags answers a primitive `ABSTRACT | FINAL |
 * PUBLIC` (0x411 = 1041) and any other class its class file's own
 * access_flags. CratonVM answered 1 (PUBLIC) for a primitive, and also for a
 * class whose class file says 0 (package-private, no ACC_SUPER -- what a
 * bytecode generator writing `access = 0` produces; the probe patches a
 * javac-compiled nested class to that), so a package-private generated class
 * read as public.
 *
 * Run (both modes must print the HotSpot lines):
 *   javac --add-exports java.base/jdk.internal.reflect=ALL-UNNAMED -d out L3W84ClassAccessFlags.java
 *   cratonvm --jdk-only   --add-exports java.base/jdk.internal.reflect=ALL-UNNAMED -cp out L3W84ClassAccessFlags
 *   cratonvm --compatible --add-exports java.base/jdk.internal.reflect=ALL-UNNAMED -cp out L3W84ClassAccessFlags
 *   java                  --add-exports java.base/jdk.internal.reflect=ALL-UNNAMED -cp out L3W84ClassAccessFlags
 *
 * Expected HotSpot 25 output (the `zero` row derived from
 * JVM_GetClassAccessFlags, which masks the parsed flags and adds nothing):
 *   int: 1041
 *   void: 1041
 *   String: 49
 *   nested: 32
 *   zero: 0 modifiers=0
 */
import java.io.InputStream;
import jdk.internal.reflect.Reflection;

public class L3W84ClassAccessFlags {
    static class Nested {}

    static class Zero {}

    /** Offset of `access_flags` in a class file (just past the constant pool). */
    static int accessFlagsOffset(byte[] b) {
        int count = ((b[8] & 0xff) << 8) | (b[9] & 0xff);
        int p = 10;
        for (int i = 1; i < count; i++) {
            int tag = b[p] & 0xff;
            switch (tag) {
                case 1 -> p += 3 + (((b[p + 1] & 0xff) << 8) | (b[p + 2] & 0xff));
                case 3, 4, 9, 10, 11, 12, 17, 18 -> p += 5;
                case 5, 6 -> { p += 9; i++; }
                case 7, 8, 16, 19, 20 -> p += 3;
                case 15 -> p += 4;
                default -> throw new IllegalStateException("tag " + tag);
            }
        }
        return p;
    }

    static final class Loader extends ClassLoader {
        Loader(ClassLoader parent) {
            super(parent);
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    public static void main(String[] args) throws Exception {
        System.out.println("int: " + Reflection.getClassAccessFlags(int.class));
        System.out.println("void: " + Reflection.getClassAccessFlags(void.class));
        System.out.println("String: " + Reflection.getClassAccessFlags(String.class));
        System.out.println("nested: " + Reflection.getClassAccessFlags(Nested.class));

        String name = "L3W84ClassAccessFlags$Zero";
        byte[] b;
        try (InputStream in = L3W84ClassAccessFlags.class.getResourceAsStream(name + ".class")) {
            b = in.readAllBytes();
        }
        int at = accessFlagsOffset(b);
        b[at] = 0;
        b[at + 1] = 0;
        Class<?> zero = new Loader(L3W84ClassAccessFlags.class.getClassLoader()).define(name, b);
        // getModifiers() reads the InnerClasses entry (package-private static
        // nested: 0x8), the class-header flags are what was patched.
        System.out.println("zero: " + Reflection.getClassAccessFlags(zero)
                + " modifiers=" + (zero.getModifiers() & ~0x8));
    }
}
