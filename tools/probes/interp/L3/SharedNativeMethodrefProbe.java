// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 7, lane L3: one `Methodref` used by BOTH an
// `invokestatic` and an `invokevirtual` in the same class, where the target is
// a static method with a registered CratonVM native
// (`java/lang/Thread.holdsLock(Ljava/lang/Object;)Z`, not an interpreter
// intrinsic). javac never emits the pair, so the class is written by a tiny
// in-process class-file writer and defined by a private loader.
//
// Both instructions share one invoke-cache key (caller class, cp index,
// is_special = false). Before wave 7 the `invokestatic` filled a cached
// `Native` entry that the `invokevirtual` then served: it popped the receiver
// as an argument and called the static native, returning a boolean instead of
// raising the linkage error.
//
// Expected (HotSpot 25), every round identical:
//   s#N: false
//   v#N: java.lang.IncompatibleClassChangeError: Expecting non-static method
//        'boolean java.lang.Thread.holdsLock(java.lang.Object)'
//   (printed on one line)
// CratonVM must match the exception class on every round; the message should
// match too (the slow path's `static_flag_mismatch` wording).
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class SharedNativeMethodrefProbe {
    static final String GEN = "L3SharedNativeMethodrefGen";

    static byte[] generate() throws IOException {
        ByteArrayOutputStream poolBytes = new ByteArrayOutputStream();
        DataOutputStream pool = new DataOutputStream(poolBytes);
        // 1..14, in order.
        utf8(pool, GEN); //                                   1
        cls(pool, 1); //                                      2
        utf8(pool, "java/lang/Object"); //                    3
        cls(pool, 3); //                                      4
        utf8(pool, "java/lang/Thread"); //                    5
        cls(pool, 5); //                                      6
        utf8(pool, "holdsLock"); //                           7
        utf8(pool, "(Ljava/lang/Object;)Z"); //               8
        pool.writeByte(12); // NameAndType                    9
        pool.writeShort(7);
        pool.writeShort(8);
        pool.writeByte(10); // Methodref Thread.holdsLock     10
        pool.writeShort(6);
        pool.writeShort(9);
        utf8(pool, "s"); //                                   11
        utf8(pool, "v"); //                                   12
        utf8(pool, "(Ljava/lang/Thread;Ljava/lang/Object;)Z"); // 13
        utf8(pool, "Code"); //                                14

        ByteArrayOutputStream out = new ByteArrayOutputStream();
        DataOutputStream d = new DataOutputStream(out);
        d.writeInt(0xCAFEBABE);
        d.writeShort(0);
        d.writeShort(52);
        d.writeShort(15);
        d.write(poolBytes.toByteArray());
        d.writeShort(0x0021); // public super
        d.writeShort(2);
        d.writeShort(4);
        d.writeShort(0); // interfaces
        d.writeShort(0); // fields
        d.writeShort(2); // methods
        // public static boolean s(Object o) { return Thread.holdsLock(o); }
        method(d, 11, 8, 1, 1, new byte[] {0x2a, (byte) 0xb8, 0, 10, (byte) 0xac});
        // public static boolean v(Thread t, Object o) — `invokevirtual #10`,
        // the SAME constant-pool entry as `s`'s `invokestatic`.
        method(d, 12, 13, 2, 2, new byte[] {0x2a, 0x2b, (byte) 0xb6, 0, 10, (byte) 0xac});
        d.writeShort(0); // attributes
        return out.toByteArray();
    }

    static void utf8(DataOutputStream pool, String s) throws IOException {
        pool.writeByte(1);
        pool.writeUTF(s);
    }

    static void cls(DataOutputStream pool, int nameIndex) throws IOException {
        pool.writeByte(7);
        pool.writeShort(nameIndex);
    }

    static void method(DataOutputStream d, int name, int desc, int maxStack, int maxLocals,
            byte[] code) throws IOException {
        d.writeShort(0x0009); // public static
        d.writeShort(name);
        d.writeShort(desc);
        d.writeShort(1);
        d.writeShort(14);
        d.writeInt(12 + code.length);
        d.writeShort(maxStack);
        d.writeShort(maxLocals);
        d.writeInt(code.length);
        d.write(code);
        d.writeShort(0); // exception table
        d.writeShort(0); // attributes
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(SharedNativeMethodrefProbe.class.getClassLoader());
        }

        Class<?> define(byte[] b) {
            return defineClass(GEN, b, 0, b.length);
        }
    }

    static String call(Method m, Object... args) {
        try {
            return String.valueOf(m.invoke(null, args));
        } catch (InvocationTargetException e) {
            Throwable t = e.getCause();
            return t.getClass().getName() + ": " + t.getMessage();
        } catch (ReflectiveOperationException e) {
            return "reflection failed: " + e;
        }
    }

    public static void main(String[] a) throws Exception {
        Class<?> gen = new Loader().define(generate());
        Method s = gen.getMethod("s", Object.class);
        Method v = gen.getMethod("v", Thread.class, Object.class);
        Object lock = new Object();
        Thread me = Thread.currentThread();
        for (int round = 1; round <= 4; round++) {
            System.out.println("s#" + round + ": " + call(s, lock));
            System.out.println("v#" + round + ": " + call(v, me, lock));
        }
    }
}
