/*
 * Interpreter round i4, wave 84, lane L3: reserved access_flags bits on a
 * field and a method (what an obfuscator may leave behind).
 *
 * JVMS 4.5 / 4.6: bits not assigned in the tables are reserved and ignored.
 * HotSpot's ClassFileParser masks a field's flags with
 * JVM_RECOGNIZED_FIELD_MODIFIERS (0x50DF) and a method's with
 * JVM_RECOGNIZED_METHOD_MODIFIERS (0x1DFF), so Field/Method/Constructor
 * .getModifiers() never show a reserved bit. CratonVM's reflection objects
 * carried the raw class-file word (0x8001 for the field below).
 *
 * The probe patches bit 0x8000 into the flags of a javac-compiled nested
 * class's field `f`, method `m` and constructor, and defines the result.
 *
 * Run (both modes must print the HotSpot lines):
 *   javac -d out L3W84ReservedModifierBits.java
 *   cratonvm --jdk-only   -cp out L3W84ReservedModifierBits
 *   cratonvm --compatible -cp out L3W84ReservedModifierBits
 *   java                  -cp out L3W84ReservedModifierBits   (HotSpot 25)
 *
 * Expected HotSpot 25 output:
 *   patched: 3
 *   field: 1
 *   method: 9
 *   ctor: 1
 */
import java.io.InputStream;
import java.util.HashMap;
import java.util.Map;

public class L3W84ReservedModifierBits {
    public static class Shape {
        public int f;

        public Shape() {}

        public static void m() {}
    }

    static int u2(byte[] b, int p) {
        return ((b[p] & 0xff) << 8) | (b[p + 1] & 0xff);
    }

    static int u4(byte[] b, int p) {
        return (u2(b, p) << 16) | u2(b, p + 2);
    }

    /** Sets bit 0x8000 on field `f`, method `m` and `<init>`; returns how many it patched. */
    static int patch(byte[] b) {
        Map<Integer, String> utf8 = new HashMap<>();
        int count = u2(b, 8);
        int p = 10;
        for (int i = 1; i < count; i++) {
            int tag = b[p] & 0xff;
            switch (tag) {
                case 1 -> {
                    int len = u2(b, p + 1);
                    utf8.put(i, new String(b, p + 3, len, java.nio.charset.StandardCharsets.UTF_8));
                    p += 3 + len;
                }
                case 3, 4, 9, 10, 11, 12, 17, 18 -> p += 5;
                case 5, 6 -> { p += 9; i++; }
                case 7, 8, 16, 19, 20 -> p += 3;
                case 15 -> p += 4;
                default -> throw new IllegalStateException("tag " + tag);
            }
        }
        p += 6; // access_flags, this_class, super_class
        p += 2 + 2 * u2(b, p); // interfaces
        int patched = 0;
        for (int kind = 0; kind < 2; kind++) { // fields, then methods
            int n = u2(b, p);
            p += 2;
            for (int i = 0; i < n; i++) {
                String name = utf8.get(u2(b, p + 2));
                if (name.equals("f") || name.equals("m") || name.equals("<init>")) {
                    b[p] |= (byte) 0x80;
                    patched++;
                }
                int attrs = u2(b, p + 6);
                p += 8;
                for (int a = 0; a < attrs; a++) {
                    p += 6 + u4(b, p + 2);
                }
            }
        }
        return patched;
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
        String name = "L3W84ReservedModifierBits$Shape";
        byte[] b;
        try (InputStream in = L3W84ReservedModifierBits.class.getResourceAsStream(name + ".class")) {
            b = in.readAllBytes();
        }
        System.out.println("patched: " + patch(b));
        Class<?> c = new Loader(L3W84ReservedModifierBits.class.getClassLoader()).define(name, b);
        System.out.println("field: " + c.getDeclaredField("f").getModifiers());
        System.out.println("method: " + c.getDeclaredMethod("m").getModifiers());
        System.out.println("ctor: " + c.getDeclaredConstructor().getModifiers());
    }
}
