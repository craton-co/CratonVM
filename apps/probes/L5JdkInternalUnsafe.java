import java.lang.reflect.Method;
import java.nio.ByteBuffer;

/**
 * L5 residual §11.3 -- a workload for the `jdk.internal.misc.Unsafe` rows that
 * carry `Code` on every supported image and that no instrument in this tree
 * dispatches.
 *
 * # Why this exists when `probes/UnsafeShadowSweep.java` already covers Unsafe
 *
 * L1's sweep is 472 rows and it is thorough, but it reaches the off-heap
 * surface through `sun.misc.Unsafe` almost everywhere: of the 26 registered
 * `jdk.internal.misc.Unsafe` triples that are declared-with-code on 17, 21 and
 * 25 and are not in one of the three permanently blocked families, its
 * `off-heap` section dispatches SEVEN (`allocateMemory`, `freeMemory`,
 * `setMemory`, `copyMemory`, `reallocateMemory`, `addressSize`, `pageSize` --
 * four of them only on their throwing edge). The other nineteen have never
 * been dispatched by anything, which is precondition 4 failing for want of a
 * caller rather than for a reason.
 *
 * The `sun.misc` spelling of this same family retired on 2026-09-11 (§9b.3)
 * once `L5SunMiscUnsafe` grew an `addrRow` helper. This file is that helper
 * pointed at the other class, and the two probes are deliberately shaped alike
 * so a difference between the spellings is visible as a difference between the
 * transcripts.
 *
 * # Reflective, like its sibling, and for a second reason
 *
 * Reaching the class by name means a method the running image does not declare
 * is a printed `EX:NoSuchMethodException` row rather than a link error that
 * takes the whole probe down -- and this class's surface moves between images
 * (25 drops `ensureClassInitialized`, renames the `Object` CAS spellings to
 * `Reference`). It also lets the same class file run on a 17, 21 or 25 image
 * without recompiling, which is how the version-boundary rows get measured.
 *
 * `--add-exports java.base/jdk.internal.misc=ALL-UNNAMED` is required: the
 * package is not exported, so `Method.invoke` throws `IllegalAccessException`
 * without it and every row reads the same. Check the `rows` trailer and the
 * `reached` count before believing a clean diff -- an arm that could not open
 * the class prints 2 rows and no difference at all.
 *
 * # What is deliberately NOT printed
 *
 * No address, no offset, no `addressSize()` VALUE, no `pageSize()` value.
 * Those are a VM's own numbering and printing one makes every row differ for a
 * reason that is not a defect. What is printed is the property that has to
 * hold whatever the numbering is: a value written through an address is the
 * value read back through it.
 */
public class L5JdkInternalUnsafe {
    static int rows;
    static int reached;
    static Object U;
    static Class<?> UC;

    static void say(String s) {
        rows++;
        System.out.println(s);
    }

    /** Every call goes through here so a throw is a ROW rather than an exit. */
    static void probe(String name, Call c) {
        String out;
        try {
            out = "ok=" + c.run();
            reached++;
        } catch (Throwable t) {
            out = unwrap(t);
        }
        say(name + " -> " + out);
    }

    /**
     * One off-heap round trip: allocate, hand the address to `c`, free.
     *
     * The `finally` is the point. Without it a row whose accessor throws leaks
     * its allocation, and a probe that leaks once per failing row stops being a
     * measurement of the accessor and starts being one of the allocator.
     */
    static void addrRow(String name, int bytes, AddrCall c) {
        String out;
        Long addr = null;
        try {
            addr = (Long) m("allocateMemory", long.class).invoke(U, (long) bytes);
            out = "ok=" + c.run(addr);
            reached++;
        } catch (Throwable t) {
            out = unwrap(t);
        } finally {
            if (addr != null) {
                try {
                    m("freeMemory", long.class).invoke(U, addr);
                } catch (Throwable ignored) {
                    // A failed free is not this row's subject, and reporting it
                    // here would attribute it to the accessor above.
                }
            }
        }
        say("addr." + name + " -> " + out);
    }

    interface AddrCall {
        Object run(long addr) throws Throwable;
    }

    interface Call {
        Object run() throws Throwable;
    }

    /** `EX:<SimpleName>` for the exception a row actually raised. */
    static String unwrap(Throwable t) {
        Throwable r = t;
        while (r instanceof java.lang.reflect.InvocationTargetException && r.getCause() != null) {
            r = r.getCause();
        }
        return "EX:" + r.getClass().getSimpleName();
    }

    static Method m(String name, Class<?>... types) throws Exception {
        return UC.getMethod(name, types);
    }

    public static void main(String[] args) throws Exception {
        try {
            UC = Class.forName("jdk.internal.misc.Unsafe");
            // `getUnsafe()` on THIS class is a plain `return theUnsafe` with no
            // caller check -- unlike the `sun.misc` spelling, which checks the
            // caller's loader and throws for an application class. So it is the
            // way in here, and no `--add-opens` is needed to reach the field.
            U = UC.getMethod("getUnsafe").invoke(null);
        } catch (Throwable t) {
            say("getUnsafe -> " + unwrap(t));
            System.out.println("reached " + reached);
            System.out.println("rows " + rows);
            System.out.println("DONE L5JdkInternalUnsafe");
            return;
        }
        say("getUnsafe -> ok=" + (U != null));

        // ---- shape-independent queries: the PROPERTY, never the number ----
        probe("addressSize", () -> ((Integer) m("addressSize").invoke(U)) > 0);
        probe("pageSize", () -> Integer.bitCount((Integer) m("pageSize").invoke(U)) == 1);

        // ---- the one fence this class has that `sun.misc.Unsafe` does not ----
        probe("loadLoadFence", () -> {
            m("loadLoadFence").invoke(U);
            return "returned";
        });

        // ---- allocate / free / reallocate, on their contract edges ----
        probe("allocateMemory(0) is zero", () ->
                ((Long) m("allocateMemory", long.class).invoke(U, 0L)) == 0L);
        probe("allocateMemory(-1)", () -> {
            long q = (Long) m("allocateMemory", long.class).invoke(U, -1L);
            m("freeMemory", long.class).invoke(U, q);
            return "RETURNED-WITHOUT-THROWING";
        });
        probe("freeMemory(0)", () -> {
            m("freeMemory", long.class).invoke(U, 0L);
            return "returned";
        });
        probe("reallocateMemory preserves the content", () -> {
            long a = (Long) m("allocateMemory", long.class).invoke(U, 16L);
            m("putLong", long.class, long.class).invoke(U, a, 0x0102030405060708L);
            long b = (Long) m("reallocateMemory", long.class, long.class).invoke(U, a, 64L);
            long seen = (Long) m("getLong", long.class).invoke(U, b);
            m("freeMemory", long.class).invoke(U, b);
            return seen == 0x0102030405060708L;
        });
        probe("reallocateMemory to 0 is zero", () -> {
            long a = (Long) m("allocateMemory", long.class).invoke(U, 16L);
            return ((Long) m("reallocateMemory", long.class, long.class).invoke(U, a, 0L)) == 0L;
        });

        // ---- the address-form accessors, one round trip each ----
        addrRow("byte", 1, a -> {
            m("putByte", long.class, byte.class).invoke(U, a, (byte) -13);
            return ((Byte) m("getByte", long.class).invoke(U, a)) == (byte) -13;
        });
        addrRow("char", 2, a -> {
            m("putChar", long.class, char.class).invoke(U, a, (char) 0x4142);
            return ((Character) m("getChar", long.class).invoke(U, a)) == (char) 0x4142;
        });
        addrRow("short", 2, a -> {
            m("putShort", long.class, short.class).invoke(U, a, (short) -31416);
            return ((Short) m("getShort", long.class).invoke(U, a)) == (short) -31416;
        });
        addrRow("int", 4, a -> {
            m("putInt", long.class, int.class).invoke(U, a, 0x0badf00d);
            return ((Integer) m("getInt", long.class).invoke(U, a)) == 0x0badf00d;
        });
        addrRow("long", 8, a -> {
            m("putLong", long.class, long.class).invoke(U, a, 0x1122334455667788L);
            return ((Long) m("getLong", long.class).invoke(U, a)) == 0x1122334455667788L;
        });
        addrRow("float", 4, a -> {
            m("putFloat", long.class, float.class).invoke(U, a, 0.5f);
            return ((Float) m("getFloat", long.class).invoke(U, a)) == 0.5f;
        });
        addrRow("double", 8, a -> {
            m("putDouble", long.class, double.class).invoke(U, a, -0.25d);
            return ((Double) m("getDouble", long.class).invoke(U, a)) == -0.25d;
        });
        // `putAddress`/`getAddress` store an address-SIZED value, so the
        // written value must fit 32 bits for the row to mean the same thing on
        // a VM that reports `addressSize() == 4`. Both VMs here report 8; the
        // row is written so that fact is not what it measures.
        addrRow("address", 8, a -> {
            m("putAddress", long.class, long.class).invoke(U, a, 0x12345678L);
            return ((Long) m("getAddress", long.class).invoke(U, a)) == 0x12345678L;
        });

        // ---- setMemory / copyMemory through the Object-base form ----
        addrRow("setMemory fills exactly its length", 16, a -> {
            m("setMemory", Object.class, long.class, long.class, byte.class)
                    .invoke(U, null, a, 16L, (byte) -1);
            m("setMemory", Object.class, long.class, long.class, byte.class)
                    .invoke(U, null, a + 2, 1L, (byte) 0);
            StringBuilder b = new StringBuilder();
            for (int i = 0; i < 4; i++) {
                b.append((Byte) m("getByte", long.class).invoke(U, a + i)).append(',');
            }
            return b.toString();
        });
        addrRow("copyMemory off-heap to off-heap", 32, a -> {
            m("putLong", long.class, long.class).invoke(U, a, 0x0102030405060708L);
            m("copyMemory", Object.class, long.class, Object.class, long.class, long.class)
                    .invoke(U, null, a, null, a + 16, 8L);
            return ((Long) m("getLong", long.class).invoke(U, a + 16)) == 0x0102030405060708L;
        });
        addrRow("copyMemory heap to off-heap and back", 16, a -> {
            byte[] src = { 1, 2, 3, 4, 5, 6, 7, 8 };
            byte[] dst = new byte[8];
            // `arrayBaseOffset(Class)` returns `int` on 17 and 21 and `long`
            // on 25 -- it is one of the three triples this class declares
            // differently across the supported images. Read it as a `Number`
            // so this row measures the copy rather than the return type.
            long base = ((Number) m("arrayBaseOffset", Class.class)
                    .invoke(U, byte[].class)).longValue();
            Method copy = m("copyMemory", Object.class, long.class, Object.class,
                            long.class, long.class);
            copy.invoke(U, src, base, null, a, 8L);
            copy.invoke(U, null, a, dst, base, 8L);
            return java.util.Arrays.toString(dst);
        });
        probe("copyMemory with a negative length", () -> {
            m("copyMemory", Object.class, long.class, Object.class, long.class, long.class)
                    .invoke(U, null, 0L, null, 0L, -1L);
            return "RETURNED-WITHOUT-THROWING";
        });

        // ---- invokeCleaner: a direct buffer's memory is released once ----
        probe("invokeCleaner on a direct buffer", () -> {
            ByteBuffer b = ByteBuffer.allocateDirect(64);
            m("invokeCleaner", ByteBuffer.class).invoke(U, b);
            return "returned";
        });
        probe("invokeCleaner refuses a heap buffer", () -> {
            m("invokeCleaner", ByteBuffer.class).invoke(U, ByteBuffer.allocate(64));
            return "RETURNED-WITHOUT-THROWING";
        });

        // ---- defineClass: define this probe's own nested class into a fresh
        // loader, so the bytes are certainly valid and the name certainly
        // matches. A second definition of the same name in the SAME loader is
        // the JDK's LinkageError, which is why a new loader is made here.
        probe("defineClass", () -> {
            String n = "L5JdkInternalUnsafe$Tiny";
            byte[] bytes;
            try (java.io.InputStream in =
                         L5JdkInternalUnsafe.class.getResourceAsStream(n + ".class")) {
                if (in == null) return "NO-CLASS-BYTES";
                bytes = in.readAllBytes();
            }
            // The name handed to `defineClass` is the BINARY name and it has
            // to match what the class file says, `$` and all. Passing the
            // dotted nested-class spelling is a `NoClassDefFoundError` with a
            // message about the mismatch, which reads exactly like a VM
            // difference and is not one.
            ClassLoader ld = new ClassLoader(L5JdkInternalUnsafe.class.getClassLoader()) { };
            Object c = m("defineClass", String.class, byte[].class, int.class, int.class,
                         ClassLoader.class, java.security.ProtectionDomain.class)
                    .invoke(U, n, bytes, 0, bytes.length, ld, null);
            return c instanceof Class && ((Class<?>) c).getName().endsWith("Tiny");
        });

        System.out.println("reached " + reached);
        System.out.println("rows " + rows);
        System.out.println("DONE L5JdkInternalUnsafe");
    }

    /** The subject of the `defineClass` row, and nothing else. */
    static class Tiny {
        int a;
    }
}
