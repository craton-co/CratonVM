// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 109, lane L7: the split (StackMapTable) verifier's texts for shapes
// earlier waves did not probe, plus the major-50 zone (a class refused by both verifiers reports
// the SPLIT verifier's message, Verifier::verify).
//   lookupswitch-unsorted   keys 5, 3: verify_switch "Bad lookupswitch instruction" (bad_code)
//   lookupswitch-duplicate  keys 3, 3: the same (keys must be strictly increasing)
//   lookupswitch-sorted     keys 3, 5: accepted
//   nonvirtual-50           aload_0; invokespecial String.length()I at major 50, no StackMapTable
//                           (OPEN: i109-L7 major-50 page; CratonVM words it as its own)
//   protected-getfield-50   p/Sub extends FilterInputStream, getfield `in` on a FilterInputStream
//                           parameter, major 50 (OPEN: same page)
//   getfield-on-int         iconst_0; getfield Integer.value
//   putstatic-float         fconst_0; putstatic Integer.MIN_VALUE (an int field)
//   invokestatic-float-arg  fconst_0; invokestatic Integer.valueOf(I)
//   aastore-int-value       iconst_1; anewarray Object; iconst_0; iconst_0; aastore
//   astore-int              iconst_0; astore_0
//   iload-float             fconst_0; fstore_0; iload_0
//   fallthrough-mismatch    fconst_0; fstore_0; return with full_frame(@2, {int}) at the return
//   smt-*                   malformed StackMapTables (wave 109 hand-back): a missing frame, a
//                           reserved frame type, a bad verification tag, an Object entry naming
//                           a Utf8, a trailing byte, a frame past the code
// (The wave-109 first cut wrote the tables without number_of_entries; fixed in the hand-back.)
// Each class is hand-assembled (major 69 unless noted), defined by a custom loader and
// initialised with Class.forName. Each row prints the header line, then the Location and Reason
// lines.
//
// Run: javac -d out L7W109SplitVerifierTexts.java && java -cp out L7W109SplitVerifierTexts
//      cratonvm [--compatible] [--nojit] -cp out L7W109SplitVerifierTexts
// HOTSPOT_EXPECTED_BEGIN (JDK 25; predicted from verifier.cpp, strings from jvm.dll)
// lookupswitch-unsorted -> java.lang.VerifyError: Bad lookupswitch instruction
//   L7W109SSwU.m()V @1: lookupswitch
//   Error exists in the bytecode
// lookupswitch-duplicate -> java.lang.VerifyError: Bad lookupswitch instruction
//   L7W109SSwDup.m()V @1: lookupswitch
//   Error exists in the bytecode
// lookupswitch-sorted -> ok
// nonvirtual-50 -> java.lang.VerifyError: Bad invokespecial instruction: current class isn't assignable to reference class.
//   L7W109SNv50.m()V @1: invokespecial
//   Error exists in the bytecode
// protected-getfield-50 -> java.lang.VerifyError: Bad access to protected data in getfield
//   p/L7W109SProt50.m(Ljava/io/FilterInputStream;)V @1: getfield
//   Type 'java/io/FilterInputStream' (current frame, stack[0]) is not assignable to 'p/L7W109SProt50'
// getfield-on-int -> java.lang.VerifyError: Bad type on operand stack
//   L7W109SGetf.m()V @1: getfield
//   Type integer (current frame, stack[0]) is not assignable to 'java/lang/Integer'
// putstatic-float -> java.lang.VerifyError: Bad type on operand stack
//   L7W109SPuts.m()V @1: putstatic
//   Type float (current frame, stack[0]) is not assignable to integer
// invokestatic-float-arg -> java.lang.VerifyError: Bad type on operand stack
//   L7W109SInvs.m()V @1: invokestatic
//   Type float (current frame, stack[0]) is not assignable to integer
// aastore-int-value -> java.lang.VerifyError: Bad type on operand stack
//   L7W109SAast.m()V @6: aastore
//   Type integer (current frame, stack[2]) is not assignable to 'java/lang/Object'
// astore-int -> java.lang.VerifyError: Bad type on operand stack
//   L7W109SAsto.m()V @1: astore_0
//   Type integer (current frame, stack[0]) is not assignable to reference type
// iload-float -> java.lang.VerifyError: Bad local variable type
//   L7W109SIlo.m()V @2: iload_0
//   Type float (current frame, locals[0]) is not assignable to integer
// fallthrough-mismatch -> java.lang.VerifyError: Instruction type does not match stack map
//   L7W109SFall.m()V @2: return
//   Type float (current frame, locals[0]) is not assignable to integer (stack map, locals[0])
// smt-truncated -> java.lang.ClassFormatError: StackMapTable format error: wrong attribute size
// smt-reserved-type -> java.lang.ClassFormatError: StackMapTable format error: reserved frame type
// smt-bad-vtype -> java.lang.ClassFormatError: StackMapTable format error: bad verification type
// smt-bad-class-index -> java.lang.ClassFormatError: StackMapTable format error: bad class index
// smt-wrong-size -> java.lang.ClassFormatError: StackMapTable format error: wrong attribute size
// smt-bad-offset -> java.lang.VerifyError: StackMapTable error: bad offset
//   L7W109SSmtOff.m()V @0: nop
//   Invalid stackmap specification.
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

public class L7W109SplitVerifierTexts {

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
            super(L7W109SplitVerifierTexts.class.getClassLoader());
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

    /** Class `name` (super Object) with `static m()V`. */
    static void staticRow(String label, String name, int major, int maxStack, int maxLocals,
            byte[] stackMap, Gen gen) throws IOException {
        Asm a = new Asm(name, "java/lang/Object", major);
        a.method(0x0009, "m", "()V", maxStack, maxLocals, gen.code(a), stackMap);
        row(label, name, a.build());
    }

    /** `iconst_0; lookupswitch {k1: +27, k2: +27, default: +27}; return` (switch at pc 1). */
    static byte[] lookupswitch(int k1, int k2) {
        return b(0x03, 0xab, 0, 0,
            0, 0, 0, 27,
            0, 0, 0, 2,
            0, 0, 0, k1, 0, 0, 0, 27,
            0, 0, 0, k2, 0, 0, 0, 27,
            0xb1);
    }

    /** number_of_entries 1, same_frame at pc 28 (the lookupswitch target). */
    static final byte[] FRAME_AT_28 = b(0, 1, 28);

    public static void main(String[] args) throws Exception {
        staticRow("lookupswitch-unsorted", "L7W109SSwU", 69, 1, 0, FRAME_AT_28,
            a -> lookupswitch(5, 3));
        staticRow("lookupswitch-duplicate", "L7W109SSwDup", 69, 1, 0, FRAME_AT_28,
            a -> lookupswitch(3, 3));
        staticRow("lookupswitch-sorted", "L7W109SSwOk", 69, 1, 0, FRAME_AT_28,
            a -> lookupswitch(3, 5));
        {
            Asm a = new Asm("L7W109SNv50", "java/lang/Object", 50);
            int m = a.methodref("java/lang/String", "length", "()I");
            a.method(0x0001, "m", "()V", 1, 1, b(0x2a, 0xb7, m >> 8, m & 0xff, 0x57, 0xb1), null);
            row("nonvirtual-50", "L7W109SNv50", a.build());
        }
        {
            Asm a = new Asm("p/L7W109SProt50", "java/io/FilterInputStream", 50);
            int f = a.fieldref("java/io/FilterInputStream", "in", "Ljava/io/InputStream;");
            a.method(0x0009, "m", "(Ljava/io/FilterInputStream;)V", 1, 1,
                b(0x2a, 0xb4, f >> 8, f & 0xff, 0x57, 0xb1), null);
            row("protected-getfield-50", "p/L7W109SProt50", a.build());
        }
        staticRow("getfield-on-int", "L7W109SGetf", 69, 1, 0, null, a -> {
            int f = a.fieldref("java/lang/Integer", "value", "I");
            return b(0x03, 0xb4, f >> 8, f & 0xff, 0x57, 0xb1);
        });
        staticRow("putstatic-float", "L7W109SPuts", 69, 1, 0, null, a -> {
            int f = a.fieldref("java/lang/Integer", "MIN_VALUE", "I");
            return b(0x0b, 0xb3, f >> 8, f & 0xff, 0xb1);
        });
        staticRow("invokestatic-float-arg", "L7W109SInvs", 69, 1, 0, null, a -> {
            int m = a.methodref("java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;");
            return b(0x0b, 0xb8, m >> 8, m & 0xff, 0x57, 0xb1);
        });
        staticRow("aastore-int-value", "L7W109SAast", 69, 3, 0, null, a -> {
            int c = a.cls("java/lang/Object");
            return b(0x04, 0xbd, c >> 8, c & 0xff, 0x03, 0x03, 0x53, 0xb1);
        });
        staticRow("astore-int", "L7W109SAsto", 69, 1, 1, null, a -> b(0x03, 0x4b, 0xb1));
        staticRow("iload-float", "L7W109SIlo", 69, 1, 1, null,
            a -> b(0x0b, 0x43, 0x1a, 0x57, 0xb1));
        // number_of_entries 1, full_frame(@2, locals {int}, stack {})
        staticRow("fallthrough-mismatch", "L7W109SFall", 69, 1, 1,
            b(0, 1, 255, 0, 2, 0, 1, 1, 0, 0), a -> b(0x0b, 0x43, 0xb1));
        // Malformed tables (StackMapReader / StackMapStream), each on `nop; nop; return`.
        staticRow("smt-truncated", "L7W109SSmtEnd", 69, 0, 0, b(0, 1), a -> b(0, 0, 0xb1));
        staticRow("smt-reserved-type", "L7W109SSmtRes", 69, 0, 0, b(0, 1, 128, 0, 0),
            a -> b(0, 0, 0xb1));
        staticRow("smt-bad-vtype", "L7W109SSmtVt", 69, 1, 0, b(0, 1, 64, 9),
            a -> b(0, 0, 0xb1));
        // cp #1 is the Utf8 "m" (the code adds no constant).
        staticRow("smt-bad-class-index", "L7W109SSmtCls", 69, 1, 0, b(0, 1, 64, 7, 0, 1),
            a -> b(0, 0, 0xb1));
        staticRow("smt-wrong-size", "L7W109SSmtSize", 69, 0, 0, b(0, 1, 0, 0),
            a -> b(0, 0, 0xb1));
        staticRow("smt-bad-offset", "L7W109SSmtOff", 69, 0, 0, b(0, 1, 5),
            a -> b(0, 0, 0xb1));
    }
}
