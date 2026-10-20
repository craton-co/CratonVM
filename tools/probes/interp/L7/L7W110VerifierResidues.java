// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 110, lane L7: verifier residues of wave 109.
//   frameless-50-type-error   major 50, no StackMapTable: iconst_0; ifeq L; nop; L: iconst_0;
//                             athrow. The split verifier refuses the ifeq (no frame at its
//                             target), libverify refuses the athrow; HotSpot reports the split one.
//   frameless-50-ok           the same with `return` at L: libverify accepts, the class loads.
//   merged-receiver-49        major 49: (b ? Integer.valueOf(1) : Long.valueOf(1)).length() as
//                             invokevirtual String.length on the merge (Number): libverify's
//                             "Incompatible object argument for function call".
//   merged-receiver-50        the same at major 50, no table: the split verifier's missing frame.
//   merged-object-49          Integer / String merge to Object (both loaded: an exact LUB), then
//                             String.length(): refused (wave 111; w110 CratonVM accepted it).
//   smt-size-full-locals      full_frame with 2 int locals, max_locals 1
//   smt-size-append           append_frame of 1 int, max_locals 0
//   smt-size-chop             chop_frame of 1 local from an empty frame
//   smt-size-stack            same_locals_1_stack_item (int), max_stack 0
// Each class is hand-assembled (major 69 unless noted), defined by a custom loader and
// initialised with Class.forName; each row prints the header line, then Location and Reason.
//
// Run: javac -d out L7W110VerifierResidues.java && java -cp out L7W110VerifierResidues
//      cratonvm [--compatible] [--nojit] -cp out L7W110VerifierResidues
// HOTSPOT_EXPECTED_BEGIN (JDK 25; predicted from verifier.cpp / check_code.c / stackMapTable.cpp)
// frameless-50-type-error -> java.lang.VerifyError: Expecting a stackmap frame at branch target 5
//   L7W110F50.m()V @1: ifeq
//   Expected stackmap frame at this location.
// frameless-50-ok -> ok
// merged-receiver-49 -> java.lang.VerifyError: (class: L7W110MR49, method: m signature: (Z)V) Incompatible object argument for function call
// merged-receiver-50 -> java.lang.VerifyError: Expecting a stackmap frame at branch target 11
//   L7W110MR50.m(Z)V @1: ifeq
//   Expected stackmap frame at this location.
// merged-object-49 -> java.lang.VerifyError: (class: L7W110MO49, method: m signature: (Z)V) Incompatible object argument for function call
// smt-size-full-locals -> java.lang.ClassFormatError: StackMapTable format error: bad type array size in method 'void L7W110SzF.m()'
// smt-size-append -> java.lang.ClassFormatError: StackMapTable format error: bad type array size in method 'void L7W110SzA.m()'
// smt-size-chop -> java.lang.ClassFormatError: StackMapTable format error: bad type array size in method 'void L7W110SzC.m()'
// smt-size-stack -> java.lang.ClassFormatError: StackMapTable format error: bad type array size in method 'void L7W110SzS.m()'
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

public class L7W110VerifierResidues {

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
            super(L7W110VerifierResidues.class.getClassLoader());
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

    /** Class `name` (super Object) with `static m<desc>`. */
    static void staticRow(String label, String name, int major, String desc, int maxStack,
            int maxLocals, byte[] stackMap, Gen gen) throws IOException {
        Asm a = new Asm(name, "java/lang/Object", major);
        a.method(0x0009, "m", desc, maxStack, maxLocals, gen.code(a), stackMap);
        row(label, name, a.build());
    }

    /**
     * `(b ? A.valueOf(..) : B.valueOf(..)).length()` as invokevirtual String.length on the merge:
     *  0 iload_0; 1 ifeq ->11; 4 iconst_1; 5 invokestatic A.valueOf; 8 goto ->15;
     *  11 <second value>; 12 invokestatic B.valueOf; 15 invokevirtual String.length; 18 pop;
     *  19 return.
     */
    static byte[] merged(Asm a, String first, String firstDesc, int second, String secondOwner,
            String secondDesc) throws IOException {
        int f = a.methodref(first, "valueOf", firstDesc);
        int s = a.methodref(secondOwner, "valueOf", secondDesc);
        int len = a.methodref("java/lang/String", "length", "()I");
        return b(0x1a, 0x99, 0, 10, 0x04, 0xb8, f >> 8, f & 0xff, 0xa7, 0, 7,
            second, 0xb8, s >> 8, s & 0xff, 0xb6, len >> 8, len & 0xff, 0x57, 0xb1);
    }

    public static void main(String[] args) throws Exception {
        // 0 iconst_0; 1 ifeq +4 (-> 5); 4 nop; 5 iconst_0; 6 athrow
        staticRow("frameless-50-type-error", "L7W110F50", 50, "()V", 1, 0, null,
            a -> b(0x03, 0x99, 0, 4, 0x00, 0x03, 0xbf));
        staticRow("frameless-50-ok", "L7W110F50Ok", 50, "()V", 1, 0, null,
            a -> b(0x03, 0x99, 0, 4, 0x00, 0xb1));
        staticRow("merged-receiver-49", "L7W110MR49", 49, "(Z)V", 2, 1, null,
            a -> merged(a, "java/lang/Integer", "(I)Ljava/lang/Integer;", 0x0a,
                "java/lang/Long", "(J)Ljava/lang/Long;"));
        staticRow("merged-receiver-50", "L7W110MR50", 50, "(Z)V", 2, 1, null,
            a -> merged(a, "java/lang/Integer", "(I)Ljava/lang/Integer;", 0x0a,
                "java/lang/Long", "(J)Ljava/lang/Long;"));
        staticRow("merged-object-49", "L7W110MO49", 49, "(Z)V", 2, 1, null,
            a -> merged(a, "java/lang/Integer", "(I)Ljava/lang/Integer;", 0x04,
                "java/lang/String", "(I)Ljava/lang/String;"));
        // full_frame(@0, locals {int, int}, stack {}) with max_locals 1
        staticRow("smt-size-full-locals", "L7W110SzF", 69, "()V", 0, 1,
            b(0, 1, 255, 0, 0, 0, 2, 1, 1, 0, 0), a -> b(0x00, 0xb1));
        // append_frame(@0, {int}) with max_locals 0
        staticRow("smt-size-append", "L7W110SzA", 69, "()V", 0, 0, b(0, 1, 252, 0, 0, 1),
            a -> b(0x00, 0xb1));
        // chop_frame(@0, 1) from the empty entry frame
        staticRow("smt-size-chop", "L7W110SzC", 69, "()V", 0, 0, b(0, 1, 250, 0, 0),
            a -> b(0x00, 0xb1));
        // same_locals_1_stack_item_frame(@0, int) with max_stack 0
        staticRow("smt-size-stack", "L7W110SzS", 69, "()V", 0, 0, b(0, 1, 64, 1),
            a -> b(0x00, 0xb1));
    }
}
