// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 112, lane L7: the last verifier residues.
//   per-value-fallback     major 49, (Z, L7W112Helper)V: a first merge of the Helper parameter
//                          (not loaded yet on CratonVM) with an Integer, popped; then an
//                          Integer / String merge (both loaded: exact Object) used as the
//                          receiver of String.length(). libverify refuses; w111 CratonVM accepted
//                          (one fallback anywhere in the method excused every Object receiver).
//   array-same-dims        major 49: (b ? new Integer[0] : new Long[0]).clone() through
//                          invokevirtual [Ljava/lang/Integer;.clone: the merge is [Number.
//   array-mixed-dims       major 49: Integer[] / int[][] merge to [Object (merge_fullinfo_types),
//                          then the same clone.
//   array-same-ok          control: Integer[] / Integer[].
//   table50-split-ok       major 50, a branch with a correct StackMapTable: loads.
//   table50-old-accepts    major 50, the table declares an int where the code has a float: the
//                          split verifier refuses the jump, libverify (which ignores the table)
//                          accepts: the class LOADS.
//   table50-both-refuse    the same table, then `iconst_0; athrow` at the target: both refuse;
//                          the split verifier's message is reported.
//   table50-pending        major 50, count 2 with one frame: a pending ClassFormatError, no
//                          failover.
//   table50-bad-offset     major 50, a frame past the code: the split verifier's VerifyError,
//                          then libverify accepts: loads.
// Run: javac -d out L7W112VerifierLast.java && java -cp out L7W112VerifierLast
//      cratonvm [--compatible] [--nojit] -cp out L7W112VerifierLast
//      CRATONVM_VERIFY_MAJOR50_FAILOVER=0 cratonvm -cp out L7W112VerifierLast   (the old walk:
//      table50-old-accepts and table50-bad-offset refuse again)
// HOTSPOT_EXPECTED_BEGIN (JDK 25; predicted from check_code.c / verifier.cpp)
// per-value-fallback -> java.lang.VerifyError: (class: L7W112PerValue, method: m signature: (ZLL7W112Helper;)V) Incompatible object argument for function call
// array-same-dims -> java.lang.VerifyError: (class: L7W112ArrSame, method: m signature: (Z)V) Incompatible object argument for function call
// array-mixed-dims -> java.lang.VerifyError: (class: L7W112ArrMixed, method: m signature: (Z)V) Incompatible object argument for function call
// array-same-ok -> ok
// table50-split-ok -> ok
// table50-old-accepts -> ok
// table50-both-refuse -> java.lang.VerifyError: Inconsistent stackmap frames at branch target 6
//   L7W112T50Both.m()V @3: ifeq
//   Type float (current frame, locals[0]) is not assignable to integer (stack map, locals[0])
// table50-pending -> java.lang.ClassFormatError: StackMapTable format error: wrong attribute size
// table50-bad-offset -> ok
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

public class L7W112VerifierLast {

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

        int member(int tag, String owner, String mname, String desc) throws IOException {
            String key = "M" + tag + owner + "." + mname + desc;
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
            cp.writeByte(tag);
            cp.writeShort(c);
            cp.writeShort(nat);
            idx.put(key, next);
            return next++;
        }

        int methodref(String owner, String mname, String desc) throws IOException {
            return member(10, owner, mname, desc);
        }

        int fieldref(String owner, String fname, String desc) throws IOException {
            return member(9, owner, fname, desc);
        }

        /** A method; `stackMap` may be null. */
        Asm method(int flags, String mname, String desc, int maxStack, int maxLocals, byte[] code,
                byte[] stackMap) throws IOException {
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
            out.writeInt(12 + code.length + smtLen);
            out.writeShort(maxStack);
            out.writeShort(maxLocals);
            out.writeInt(code.length);
            out.write(code);
            out.writeShort(0);
            out.writeShort(stackMap == null ? 0 : 1);
            if (stackMap != null) {
                out.writeShort(smtU);
                out.writeInt(stackMap.length);
                out.write(stackMap);
            }
            methods.add(bytes.toByteArray());
            return this;
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
            super(L7W112VerifierLast.class.getClassLoader());
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

    static void row(String label, String internalName, byte[] bytes) {
        String binaryName = internalName.replace('/', '.');
        Map<String, byte[]> all = new HashMap<>();
        all.put(binaryName, bytes);
        all.putAll(EXTRA);
        try {
            Class.forName(binaryName, true, new Loader(all));
            System.out.println(label + " -> ok");
        } catch (Throwable t) {
            String msg = String.valueOf(t.getMessage());
            String[] lines = msg.split("\n");
            System.out.println(label + " -> " + t.getClass().getName() + ": " + lines[0]);
            for (int i = 0; i + 1 < lines.length; i++) {
                String l = lines[i].trim();
                if (l.equals("Location:") || l.equals("Reason:")) {
                    System.out.println("  " + lines[i + 1].trim());
                }
            }
        }
    }

    interface Gen {
        byte[] code(Asm a) throws IOException;
    }

    /** Classes every row's loader can also define (loaded only when asked for). */
    static final Map<String, byte[]> EXTRA = new HashMap<>();

    static void staticRow(String label, String name, int major, String desc, int maxStack,
            int maxLocals, byte[] stackMap, Gen gen) throws IOException {
        Asm a = new Asm(name, "java/lang/Object", major);
        a.method(0x0009, "m", desc, maxStack, maxLocals, gen.code(a), stackMap);
        row(label, name, a.build());
    }

    /** `b ? <first array> : <second array>`, then invokevirtual [Ljava/lang/Integer;.clone. */
    static byte[] arrays(Asm a, String first, String second) throws IOException {
        int f = a.cls(first);
        int s = a.cls(second);
        int clone = a.methodref("[Ljava/lang/Integer;", "clone", "()Ljava/lang/Object;");
        //  0 iload_0; 1 ifeq ->11; 4 iconst_0; 5 anewarray first; 8 goto ->15;
        // 11 iconst_0; 12 anewarray second; 15 invokevirtual clone; 18 pop; 19 return
        return b(0x1a, 0x99, 0, 10, 0x03, 0xbd, f >> 8, f & 0xff, 0xa7, 0, 7,
            0x03, 0xbd, s >> 8, s & 0xff, 0xb6, clone >> 8, clone & 0xff, 0x57, 0xb1);
    }

    public static void main(String[] args) throws Exception {
        EXTRA.put("L7W112Helper", new Asm("L7W112Helper", "java/lang/Object", 49).build());
        staticRow("per-value-fallback", "L7W112PerValue", 49, "(ZLL7W112Helper;)V", 1, 2, null,
            a -> {
                int iv = a.methodref("java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;");
                int sv = a.methodref("java/lang/String", "valueOf", "(I)Ljava/lang/String;");
                int len = a.methodref("java/lang/String", "length", "()I");
                //  0 iload_0; 1 ifeq ->8; 4 aload_1; 5 goto ->12; 8 iconst_1;
                //  9 invokestatic Integer.valueOf; 12 pop (Helper / Integer merge);
                // 13 iload_0; 14 ifeq ->24; 17 iconst_1; 18 invokestatic Integer.valueOf;
                // 21 goto ->28; 24 iconst_1; 25 invokestatic String.valueOf;
                // 28 invokevirtual String.length; 31 pop; 32 return
                return b(0x1a, 0x99, 0, 7, 0x2b, 0xa7, 0, 7, 0x04, 0xb8, iv >> 8, iv & 0xff,
                    0x57, 0x1a, 0x99, 0, 10, 0x04, 0xb8, iv >> 8, iv & 0xff, 0xa7, 0, 7,
                    0x04, 0xb8, sv >> 8, sv & 0xff, 0xb6, len >> 8, len & 0xff, 0x57, 0xb1);
            });
        staticRow("array-same-dims", "L7W112ArrSame", 49, "(Z)V", 1, 1, null,
            a -> arrays(a, "java/lang/Integer", "java/lang/Long"));
        staticRow("array-mixed-dims", "L7W112ArrMixed", 49, "(Z)V", 1, 1, null,
            a -> arrays(a, "java/lang/Integer", "[I"));
        staticRow("array-same-ok", "L7W112ArrOk", 49, "(Z)V", 1, 1, null,
            a -> arrays(a, "java/lang/Integer", "java/lang/Integer"));
        // 0 iconst_0; 1 ifeq ->5; 4 nop; 5 return; same_frame(@5)
        staticRow("table50-split-ok", "L7W112T50Ok", 50, "()V", 1, 1, b(0, 1, 5),
            a -> b(0x03, 0x99, 0, 4, 0x00, 0xb1));
        // 0 fconst_0; 1 fstore_0; 2 iconst_0; 3 ifeq ->6; 6 return; full_frame(@6, {int}, {})
        staticRow("table50-old-accepts", "L7W112T50Old", 50, "()V", 1, 1,
            b(0, 1, 255, 0, 6, 0, 1, 1, 0, 0), a -> b(0x0b, 0x43, 0x03, 0x99, 0, 3, 0xb1));
        // ... 6 iconst_0; 7 athrow
        staticRow("table50-both-refuse", "L7W112T50Both", 50, "()V", 1, 1,
            b(0, 1, 255, 0, 6, 0, 1, 1, 0, 0), a -> b(0x0b, 0x43, 0x03, 0x99, 0, 3, 0x03, 0xbf));
        staticRow("table50-pending", "L7W112T50Pend", 50, "()V", 0, 0, b(0, 2, 0),
            a -> b(0x00, 0xb1));
        staticRow("table50-bad-offset", "L7W112T50Off", 50, "()V", 0, 0, b(0, 1, 9),
            a -> b(0x00, 0xb1));
    }
}
