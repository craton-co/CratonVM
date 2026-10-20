// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 109, lane L7: libverify's (check_code.c, class files below major 50)
// VerifyError texts for the faults wave 102's L7W102VerifyOldTexts did not cover. The reasons are
// the strings JDK 25's verify.dll carries; the message is
// `(class: C, method: m signature: D) <reason>` (print_CCerror_info + CCerror).
//   ldc-class-48          ldc <CONSTANT_Class> at major 48 (LDC_CLASS_MAJOR_VERSION is 49)
//   ldc-class-49          the same at major 49: accepted
//   init-on-int           iconst_0; invokespecial Object.<init>   ('@' operand, "uninitialized")
//   iload-short-past-max  iload_0 with max_locals 0 (short forms are checked like iload n)
//   lstore-short-past-max lconst_0; lstore_0 with max_locals 1
//   register-wrong-type   fstore_0; iload_0
//   pair-uninitialized    istore_0; lload_0
//   pair-wrong-type       istore_0; istore_1; lload_0
//   stack-height          iconst_0; ifeq L; iconst_1; L: return
//   nonvirtual            aload_0; invokespecial String.length()I from an Object subclass
//   invokevirtual-init    aload_0; invokevirtual Object.<init>()V
//   methodref-clinit      invokestatic Object.<clinit>()V: the class-file parser's ClassFormatError
//                         (OPEN: i109-L7 page methodref-named-clinit-passes-the-parser)
//   invokeinterface-clinit aconst_null; invokeinterface Runnable.<clinit>()V
//   athrow-int            iconst_0; athrow
//   monitorenter-int      iconst_0; monitorenter
//   protected-getfield    p/Sub extends FilterInputStream: getfield FilterInputStream.in on a
//                         FilterInputStream parameter (old text, then the split one at major 69)
//   lookupswitch-*        unsorted keys, an illegal default, an illegal case target
//   incompatible-receiver new Object; invokevirtual String.length()I (OPEN: i109-L7 page
//                         old-verifier-accepts-unassignable-receivers; CratonVM says "ok")
// Each class is hand-assembled, defined by a custom loader and initialised with Class.forName.
//
// Run: javac -d out L7W109OldVerifierTexts.java && java -cp out L7W109OldVerifierTexts
//      cratonvm [--compatible] [--nojit] -cp out L7W109OldVerifierTexts
// HOTSPOT_EXPECTED_BEGIN (JDK 25; reasons from verify.dll / check_code.c)
// ldc-class-48 -> java.lang.VerifyError: (class: L7W109OLdc48, method: m signature: ()V) Illegal type in constant pool
// ldc-class-49 -> ok
// init-on-int -> java.lang.VerifyError: (class: L7W109OInit, method: m signature: ()V) Expecting to find uninitialized object on stack
// iload-short-past-max -> java.lang.VerifyError: (class: L7W109OIload, method: m signature: ()V) Illegal local variable number
// lstore-short-past-max -> java.lang.VerifyError: (class: L7W109OLstore, method: m signature: ()V) Illegal local variable number
// register-wrong-type -> java.lang.VerifyError: (class: L7W109OReg, method: m signature: ()V) Register 0 contains wrong type
// pair-uninitialized -> java.lang.VerifyError: (class: L7W109OPairU, method: m signature: ()V) Accessing value from uninitialized register pair 0/1
// pair-wrong-type -> java.lang.VerifyError: (class: L7W109OPairW, method: m signature: ()V) Register pair 0/1 contains wrong type
// stack-height -> java.lang.VerifyError: (class: L7W109OHeight, method: m signature: ()V) Inconsistent stack height 1 != 0
// nonvirtual -> java.lang.VerifyError: (class: L7W109ONv, method: m signature: ()V) Illegal use of nonvirtual function call
// invokevirtual-init -> java.lang.VerifyError: (class: L7W109OVInit, method: m signature: ()V) Must call initializers using invokespecial
// methodref-clinit -> java.lang.ClassFormatError: Bad method name at constant pool index 3 in class file L7W109OClinit
// invokeinterface-clinit -> java.lang.VerifyError: (class: L7W109OIClinit, method: m signature: ()V) Illegal call to internal method
// athrow-int -> java.lang.VerifyError: (class: L7W109OThrow, method: m signature: ()V) Expecting to find object on stack
// monitorenter-int -> java.lang.VerifyError: (class: L7W109OMon, method: m signature: ()V) Expecting to find object/array on stack
// protected-getfield -> java.lang.VerifyError: (class: p/L7W109OProt, method: m signature: (Ljava/io/FilterInputStream;)V) Bad access to protected data
// protected-getfield-69 -> java.lang.VerifyError: Bad access to protected data in getfield
//   p/L7W109OProt69.m(Ljava/io/FilterInputStream;)V @1: getfield
//   Type 'java/io/FilterInputStream' (current frame, stack[0]) is not assignable to 'p/L7W109OProt69'
// lookupswitch-unsorted -> java.lang.VerifyError: (class: L7W109OSwU, method: m signature: ()V) Unsorted lookup switch
// lookupswitch-bad-default -> java.lang.VerifyError: (class: L7W109OSwD, method: m signature: ()V) Illegal default target in switch
// lookupswitch-bad-case -> java.lang.VerifyError: (class: L7W109OSwC, method: m signature: ()V) Illegal branch in tableswitch
// lookupswitch-sorted -> ok
// incompatible-receiver -> java.lang.VerifyError: (class: L7W109ORecv, method: m signature: ()V) Incompatible object argument for function call
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

public class L7W109OldVerifierTexts {

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
            super(L7W109OldVerifierTexts.class.getClassLoader());
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
            Gen gen) throws IOException {
        Asm a = new Asm(name, "java/lang/Object", major);
        a.method(0x0009, "m", "()V", maxStack, maxLocals, gen.code(a), null);
        row(label, name, a.build());
    }

    /** Class `name` (super Object) with an instance `m()V` (max_locals 1). */
    static void instanceRow(String label, String name, int major, int maxStack, Gen gen)
            throws IOException {
        Asm a = new Asm(name, "java/lang/Object", major);
        a.method(0x0001, "m", "()V", maxStack, 1, gen.code(a), null);
        row(label, name, a.build());
    }

    /** `iconst_0; lookupswitch {k1: +o1, k2: +o2, default: +d}; return` (switch at pc 1). */
    static byte[] lookupswitch(int k1, int o1, int k2, int o2, int d) {
        return b(0x03, 0xab, 0, 0,
            0, 0, 0, d,
            0, 0, 0, 2,
            0, 0, 0, k1, 0, 0, 0, o1,
            0, 0, 0, k2, 0, 0, 0, o2,
            0xb1);
    }

    static byte[] protectedClass(String name, int major) throws IOException {
        Asm a = new Asm(name, "java/io/FilterInputStream", major);
        int f = a.fieldref("java/io/FilterInputStream", "in", "Ljava/io/InputStream;");
        a.method(0x0009, "m", "(Ljava/io/FilterInputStream;)V", 1, 1,
            b(0x2a, 0xb4, f >> 8, f & 0xff, 0x57, 0xb1), null);
        return a.build();
    }

    public static void main(String[] args) throws Exception {
        staticRow("ldc-class-48", "L7W109OLdc48", 48, 1, 0, a -> {
            int c = a.cls("java/lang/Object");
            return b(0x12, c, 0x57, 0xb1);
        });
        staticRow("ldc-class-49", "L7W109OLdc49", 49, 1, 0, a -> {
            int c = a.cls("java/lang/Object");
            return b(0x12, c, 0x57, 0xb1);
        });
        staticRow("init-on-int", "L7W109OInit", 49, 1, 0, a -> {
            int m = a.methodref("java/lang/Object", "<init>", "()V");
            return b(0x03, 0xb7, m >> 8, m & 0xff, 0xb1);
        });
        staticRow("iload-short-past-max", "L7W109OIload", 49, 1, 0, a -> b(0x1a, 0x57, 0xb1));
        staticRow("lstore-short-past-max", "L7W109OLstore", 49, 2, 1, a -> b(0x09, 0x3f, 0xb1));
        staticRow("register-wrong-type", "L7W109OReg", 49, 1, 1,
            a -> b(0x0b, 0x43, 0x1a, 0x57, 0xb1));
        staticRow("pair-uninitialized", "L7W109OPairU", 49, 2, 2,
            a -> b(0x03, 0x3b, 0x1e, 0x58, 0xb1));
        staticRow("pair-wrong-type", "L7W109OPairW", 49, 2, 2,
            a -> b(0x03, 0x3b, 0x03, 0x3c, 0x1e, 0x58, 0xb1));
        // 0: iconst_0  1: ifeq +4 (-> 5)  4: iconst_1  5: return
        staticRow("stack-height", "L7W109OHeight", 49, 1, 0,
            a -> b(0x03, 0x99, 0x00, 0x04, 0x04, 0xb1));
        instanceRow("nonvirtual", "L7W109ONv", 49, 1, a -> {
            int m = a.methodref("java/lang/String", "length", "()I");
            return b(0x2a, 0xb7, m >> 8, m & 0xff, 0x57, 0xb1);
        });
        instanceRow("invokevirtual-init", "L7W109OVInit", 49, 1, a -> {
            int m = a.methodref("java/lang/Object", "<init>", "()V");
            return b(0x2a, 0xb6, m >> 8, m & 0xff, 0xb1);
        });
        // A Methodref named <clinit> is refused by the class-file parser (its first
        // constant pool entries: #1 "java/lang/Object", #2 Class, #3 "<clinit>").
        staticRow("methodref-clinit", "L7W109OClinit", 49, 0, 0, a -> {
            int m = a.methodref("java/lang/Object", "<clinit>", "()V");
            return b(0xb8, m >> 8, m & 0xff, 0xb1);
        });
        // An InterfaceMethodref is not: the verifier's "Illegal call to internal method".
        staticRow("invokeinterface-clinit", "L7W109OIClinit", 49, 1, 0, a -> {
            int m = a.member(11, "java/lang/Runnable", "<clinit>", "()V");
            return b(0x01, 0xb9, m >> 8, m & 0xff, 1, 0, 0xb1);
        });
        staticRow("athrow-int", "L7W109OThrow", 49, 1, 0, a -> b(0x03, 0xbf));
        staticRow("monitorenter-int", "L7W109OMon", 49, 1, 0, a -> b(0x03, 0xc2, 0xb1));
        row("protected-getfield", "p/L7W109OProt", protectedClass("p/L7W109OProt", 49));
        row("protected-getfield-69", "p/L7W109OProt69", protectedClass("p/L7W109OProt69", 69));
        // Targets are relative to the switch at pc 1; pc 28 is the return, pc 2 is padding.
        staticRow("lookupswitch-unsorted", "L7W109OSwU", 49, 1, 0,
            a -> lookupswitch(5, 27, 3, 27, 27));
        staticRow("lookupswitch-bad-default", "L7W109OSwD", 49, 1, 0,
            a -> lookupswitch(3, 27, 5, 27, 1));
        staticRow("lookupswitch-bad-case", "L7W109OSwC", 49, 1, 0,
            a -> lookupswitch(3, 27, 5, 1, 27));
        staticRow("lookupswitch-sorted", "L7W109OSwOk", 49, 1, 0,
            a -> lookupswitch(3, 27, 5, 27, 27));
        staticRow("incompatible-receiver", "L7W109ORecv", 49, 2, 0, a -> {
            int c = a.cls("java/lang/Object");
            int init = a.methodref("java/lang/Object", "<init>", "()V");
            int len = a.methodref("java/lang/String", "length", "()I");
            return b(0xbb, c >> 8, c & 0xff, 0x59, 0xb7, init >> 8, init & 0xff,
                0xb6, len >> 8, len & 0xff, 0x57, 0xb1);
        });
    }
}
