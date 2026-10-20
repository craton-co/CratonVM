// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 108, lane L7 (hunt): HotSpot's texts for an `ldc` / `ldc_w` /
// `ldc2_w` whose constant pool operand is outside the pool or of a tag the opcode does not load
// (`ClassVerifier::verify_ldc` -> `verify_cp_index` / `verify_cp_type`, `bad_cp_index`), and
// libverify's below major 50 (`verify_constant_pool_type`). CratonVM refused all of them with its
// own text ("ldc: invalid constant pool index N") before wave 108.
//   ldc-long            ldc #<CONSTANT_Long>              major 69
//   ldc-w-out-of-pool   ldc_w #32767                     major 69
//   ldc2w-int           ldc2_w #<CONSTANT_Integer>       major 69
//   old-ldc-long        ldc #<CONSTANT_Long>              major 49 (libverify)
//   ldc-int             ldc #<CONSTANT_Integer>; pop      control, ok
// Each class is hand-assembled and initialised with Class.forName (link = verify).
//
// Run: javac -d out L7W108LdcCpTypes.java && java -cp out L7W108LdcCpTypes
//      cratonvm [--compatible] [--nojit] -cp out L7W108LdcCpTypes
// Each row prints the header line, then the Location and Reason lines (constant pool indices
// replaced by '#').
// HOTSPOT_EXPECTED_BEGIN (JDK 25; predicted from verifier.cpp / check_code.c)
// ldc-long -> java.lang.VerifyError: Illegal type at constant pool entry # in class L7W108LcLong
//   L7W108LcLong.m()V @0: ldc
//   Constant pool index # is invalid
// ldc-w-out-of-pool -> java.lang.VerifyError: Illegal constant pool index 32767 in class L7W108LcOut
//   L7W108LcOut.m()V @0: ldc_w
//   Constant pool index 32767 is invalid
// ldc2w-int -> java.lang.VerifyError: Illegal type at constant pool entry # in class L7W108LcInt2
//   L7W108LcInt2.m()V @0: ldc2_w
//   Constant pool index # is invalid
// old-ldc-long -> java.lang.VerifyError: (class: L7W108LcOld, method: m signature: ()V) Illegal type in constant pool
// ldc-int -> ok
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

public class L7W108LdcCpTypes {

    /** A minimal class-file writer: one class, any number of methods. */
    static final class Asm {
        final ByteArrayOutputStream cpBytes = new ByteArrayOutputStream();
        final DataOutputStream cp = new DataOutputStream(cpBytes);
        final Map<String, Integer> idx = new HashMap<>();
        final List<byte[]> methods = new ArrayList<>();
        int next = 1;
        final String name;
        final String superName;
        final int major;

        Asm(String name, String superName, int major) {
            this.name = name;
            this.superName = superName;
            this.major = major;
        }

        int utf8(String s) throws IOException {
            Integer i = idx.get("U" + s);
            if (i != null) {
                return i;
            }
            cp.writeByte(1);
            cp.writeUTF(s);
            idx.put("U" + s, next);
            return next++;
        }

        int cls(String n) throws IOException {
            Integer i = idx.get("C" + n);
            if (i != null) {
                return i;
            }
            int u = utf8(n);
            cp.writeByte(7);
            cp.writeShort(u);
            idx.put("C" + n, next);
            return next++;
        }

        int methodref(String owner, String mname, String desc) throws IOException {
            String key = "M" + owner + "." + mname + desc;
            Integer i = idx.get(key);
            if (i != null) {
                return i;
            }
            int c = cls(owner);
            int n = utf8(mname);
            int d = utf8(desc);
            cp.writeByte(12);
            cp.writeShort(n);
            cp.writeShort(d);
            int nat = next++;
            cp.writeByte(10);
            cp.writeShort(c);
            cp.writeShort(nat);
            idx.put(key, next);
            return next++;
        }

        int longConst(long v) throws IOException {
            cp.writeByte(5);
            cp.writeLong(v);
            int at = next;
            next += 2;
            return at;
        }

        int intConst(int v) throws IOException {
            cp.writeByte(3);
            cp.writeInt(v);
            return next++;
        }

        /** A method; `stackMap` may be null; handlers are {start, end, handler, catchType}. */
        Asm method(int flags, String mname, String desc, int maxStack, int maxLocals, byte[] code,
                byte[] stackMap, int[][] handlers) throws IOException {
            int nameU = utf8(mname);
            int descU = utf8(desc);
            int codeU = utf8("Code");
            int smtU = stackMap == null ? 0 : utf8("StackMapTable");
            ByteArrayOutputStream bytes = new ByteArrayOutputStream();
            DataOutputStream out = new DataOutputStream(bytes);
            out.writeShort(flags);
            out.writeShort(nameU);
            out.writeShort(descU);
            out.writeShort(1);
            out.writeShort(codeU);
            int smtLen = stackMap == null ? 0 : 6 + stackMap.length;
            out.writeInt(12 + code.length + 8 * handlers.length + smtLen);
            out.writeShort(maxStack);
            out.writeShort(maxLocals);
            out.writeInt(code.length);
            out.write(code);
            out.writeShort(handlers.length);
            for (int[] h : handlers) {
                for (int v : h) {
                    out.writeShort(v);
                }
            }
            out.writeShort(stackMap == null ? 0 : 1);
            if (stackMap != null) {
                out.writeShort(smtU);
                out.writeInt(stackMap.length);
                out.write(stackMap);
            }
            methods.add(bytes.toByteArray());
            return this;
        }

        /** `<init>()V` calling `target.<init>()V` on `this`. */
        Asm ctor(String target) throws IOException {
            int m = methodref(target, "<init>", "()V");
            return method(0x0001, "<init>", "()V", 1, 1, b(0x2a, 0xb7, m >> 8, m & 0xff, 0xb1), null,
                new int[0][]);
        }

        byte[] build() throws IOException {
            int thisC = cls(name);
            int superC = cls(superName);
            ByteArrayOutputStream bytes = new ByteArrayOutputStream();
            DataOutputStream out = new DataOutputStream(bytes);
            out.writeInt(0xCAFEBABE);
            out.writeShort(0);
            out.writeShort(major);
            out.writeShort(next);
            cp.flush();
            out.write(cpBytes.toByteArray());
            out.writeShort(0x21);
            out.writeShort(thisC);
            out.writeShort(superC);
            out.writeShort(0);
            out.writeShort(0);
            out.writeShort(methods.size());
            for (byte[] m : methods) {
                out.write(m);
            }
            out.writeShort(0);
            return bytes.toByteArray();
        }
    }

    static byte[] b(int... v) {
        byte[] r = new byte[v.length];
        for (int i = 0; i < v.length; i++) {
            r[i] = (byte) v[i];
        }
        return r;
    }

    static final class Loader extends ClassLoader {
        final Map<String, byte[]> classes;

        Loader(Map<String, byte[]> classes) {
            super(L7W108LdcCpTypes.class.getClassLoader());
            this.classes = classes;
        }

        @Override
        protected Class<?> findClass(String n) throws ClassNotFoundException {
            byte[] bytes = classes.get(n);
            if (bytes == null) {
                throw new ClassNotFoundException(n);
            }
            return defineClass(n, bytes, 0, bytes.length);
        }
    }

    static void row(String label, String binaryName, byte[] bytes) {
        Map<String, byte[]> all = new HashMap<>();
        all.put(binaryName, bytes);
        try {
            Class.forName(binaryName, true, new Loader(all));
            System.out.println(label + " -> ok");
        } catch (Throwable t) {
            String msg = String.valueOf(t.getMessage());
            String[] lines = msg.split("\n");
            System.out.println(label + " -> " + t.getClass().getName() + ": "
                + lines[0].replaceAll("entry \\d+ in", "entry # in"));
            for (int i = 0; i + 1 < lines.length; i++) {
                String l = lines[i].trim();
                if (l.equals("Location:") || l.equals("Reason:")) {
                    System.out.println("  " + lines[i + 1].trim()
                        .replaceAll("index (\\d{1,4}) is invalid", "index # is invalid"));
                }
            }
        }
    }

    interface Gen {
        byte[] code(Asm a) throws IOException;
    }

    static byte[] withM(String name, int major, int maxStack, Gen gen) throws IOException {
        Asm a = new Asm(name, "java/lang/Object", major).ctor("java/lang/Object");
        a.method(0x0009, "m", "()V", maxStack, 0, gen.code(a), null, new int[0][]);
        return a.build();
    }

    public static void main(String[] args) throws Exception {
        row("ldc-long", "L7W108LcLong", withM("L7W108LcLong", 69, 2, a -> {
            int k = a.longConst(7L);
            return b(0x12, k, 0x57, 0xb1);
        }));
        row("ldc-w-out-of-pool", "L7W108LcOut", withM("L7W108LcOut", 69, 2, a ->
            b(0x13, 0x7f, 0xff, 0x57, 0xb1)));
        row("ldc2w-int", "L7W108LcInt2", withM("L7W108LcInt2", 69, 2, a -> {
            int k = a.intConst(7);
            return b(0x14, k >> 8, k & 0xff, 0x58, 0xb1);
        }));
        row("old-ldc-long", "L7W108LcOld", withM("L7W108LcOld", 49, 2, a -> {
            int k = a.longConst(7L);
            return b(0x12, k, 0x57, 0xb1);
        }));
        row("ldc-int", "L7W108LcIntOk", withM("L7W108LcIntOk", 69, 1, a -> {
            int k = a.intConst(7);
            return b(0x12, k, 0x57, 0xb1);
        }));
    }
}
