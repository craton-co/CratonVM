// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, wave 110, lane L5 (page
// interpreter-i109-L5-classfileparser-checks-not-made-FIXED-20261017) -- more of HotSpot 25's
// `ClassFileParser` ClassFormatErrors for class files a user-defined loader
// hands to `defineClass` (`_need_verify` on): constant-tag versions, the
// constant pool's reference and format passes, SourceFile / EnclosingMethod /
// BootstrapMethods / Exceptions attributes, a Code attribute's own attribute
// names, and the LocalVariable(Type)Table duplicate rules. Each class is
// hand-assembled from raw bytes (`public class X extends Object` with one
// static method `m()V`, version 69 unless the row says otherwise) with exactly
// one defect, defined by a fresh loader under its binary name; the row prints
// `Throwable.toString()`. CratonVM DEFINED most of these classes before wave
// 110 (the rest were refused in the reader's words).
//
// Run (no setup):
//   javac -d out L5W110ClassFileParserMore.java
//   java -cp out L5W110ClassFileParserMore                             (HotSpot 25)
//   cratonvm --jdk-only   [--nojit] -cp out L5W110ClassFileParserMore
//   cratonvm --compatible [--nojit] -cp out L5W110ClassFileParserMore
//
// HOTSPOT_EXPECTED predicted from the JDK 25 sources (classFileParser.cpp), not run.
// HOTSPOT_EXPECTED_BEGIN
// ok-indy -> defined L5W110Ok
// tag-mh-old -> java.lang.ClassFormatError: Class file version does not support constant tag 15 in class file L5W110To
// tag-condy-old -> java.lang.ClassFormatError: Class file version does not support constant tag 17 in class file L5W110Tc
// tag-module-old -> java.lang.ClassFormatError: Unknown constant tag 19 in class file L5W110Tm
// package-tag-in-class -> java.lang.ClassFormatError: Unknown constant tag 20 in class file L5W110Tp
// fieldref-class -> java.lang.ClassFormatError: Invalid constant pool index 1 in class file L5W110Fc
// methodref-nat -> java.lang.ClassFormatError: Invalid constant pool index 2 in class file L5W110Mt
// nat-name -> java.lang.ClassFormatError: Invalid constant pool index 2 in class file L5W110Nn
// methodtype-desc -> java.lang.ClassFormatError: Invalid constant pool index 2 in class file L5W110Md
// indy-nat -> java.lang.ClassFormatError: Invalid constant pool index 2 in class file L5W110In
// mh-kind -> java.lang.ClassFormatError: Bad method handle kind at constant pool index 9 in class file L5W110Hk
// mh-not-field -> java.lang.ClassFormatError: Invalid constant pool index 8 in class file L5W110Hf (not a field)
// mh-not-interface -> java.lang.ClassFormatError: Invalid constant pool index 8 in class file L5W110Hi (not an interface method)
// nat-zero-length -> java.lang.ClassFormatError: Illegal zero length constant pool entry at 6 in class L5W110Nz
// nat-method-name -> java.lang.ClassFormatError: Illegal method name "a.b" in class L5W110Nm
// nat-field-sig -> java.lang.ClassFormatError: Field "f" in class L5W110Ns has illegal signature "Q"
// fieldref-method-sig -> java.lang.ClassFormatError: Field "f" in class L5W110Fm has illegal signature "()V"
// methodref-field-sig -> java.lang.ClassFormatError: Method "m2" in class L5W110Mf has illegal signature "I"
// sourcefile-twice -> java.lang.ClassFormatError: Multiple SourceFile attributes in class file L5W110St
// sourcefile-index -> java.lang.ClassFormatError: Invalid SourceFile attribute at constant pool index 2 in class file L5W110Si
// sourcefile-length -> java.lang.ClassFormatError: Wrong SourceFile attribute length in class file L5W110Sl
// enclosing-twice -> java.lang.ClassFormatError: Multiple EnclosingMethod attributes in class file L5W110Et
// enclosing-class-0 -> java.lang.ClassFormatError: Invalid class index in EnclosingMethod attribute in class file L5W110E0
// enclosing-class-bad -> java.lang.ClassFormatError: Invalid or out-of-bounds class index in EnclosingMethod attribute in class file L5W110Ec
// enclosing-method-bad -> java.lang.ClassFormatError: Invalid or out-of-bounds method index in EnclosingMethod attribute in class file L5W110Em
// bsm-missing -> java.lang.ClassFormatError: Missing BootstrapMethods attribute in class file L5W110Bm
// bsm-short -> java.lang.ClassFormatError: Short length on BootstrapMethods in class file L5W110Bs
// bsm-twice -> java.lang.ClassFormatError: Multiple BootstrapMethods attributes in class file L5W110Bt
// bsm-method-type -> java.lang.ClassFormatError: bootstrap_method_index 2 has bad constant type in class file L5W110Bh
// bsm-arg -> java.lang.ClassFormatError: argument_index 1 has bad constant type in class file L5W110Ba
// exceptions-twice -> java.lang.ClassFormatError: Multiple Exceptions attributes in class file L5W110Xt
// exceptions-type -> java.lang.ClassFormatError: Exception name has bad type at constant pool 1 in class file L5W110Xb
// exceptions-length -> java.lang.ClassFormatError: Exceptions attribute has wrong length in class file L5W110Xl
// code-attr-name -> java.lang.ClassFormatError: Invalid code attribute name index 2 in class file L5W110Ca
// lvt-dup -> java.lang.ClassFormatError: Duplicated LocalVariableTable attribute entry for 'x' in class file L5W110Ld
// lvtt-unmatched -> java.lang.ClassFormatError: LVTT entry for 'y' in class file L5W110Lu does not match any LVT entry
// lvtt-dup -> java.lang.ClassFormatError: Duplicated LocalVariableTypeTable attribute entry for 'x' in class file L5W110Lt
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.util.ArrayList;
import java.util.List;

public class L5W110ClassFileParserMore {

    /** Raw class-file writer; constant-pool entries are appended in call order, never shared. */
    static final class K {
        final ByteArrayOutputStream cpBytes = new ByteArrayOutputStream();
        final DataOutputStream cp = new DataOutputStream(cpBytes);
        int next = 1;
        final int major;
        int flags = 0x21;
        final int thisIdx;
        final int superIdx;
        final List<byte[]> methods = new ArrayList<>();
        final List<byte[]> attrs = new ArrayList<>();

        /** 1 Utf8 name, 2 its Class, 3 Utf8 java/lang/Object, 4 its Class. */
        K(String name, int major) throws IOException {
            this.major = major;
            thisIdx = cls(utf8(name));
            superIdx = cls(utf8("java/lang/Object"));
        }

        K(String name) throws IOException {
            this(name, 69);
        }

        int utf8(String s) throws IOException {
            cp.writeByte(1);
            cp.writeUTF(s);
            return next++;
        }

        int cls(int nameIdx) throws IOException {
            cp.writeByte(7);
            cp.writeShort(nameIdx);
            return next++;
        }

        int two(int tag, int a, int b) throws IOException {
            cp.writeByte(tag);
            cp.writeShort(a);
            cp.writeShort(b);
            return next++;
        }

        int nat(int name, int desc) throws IOException {
            return two(12, name, desc);
        }

        int fieldref(int c, int nat) throws IOException {
            return two(9, c, nat);
        }

        int methodref(int c, int nat) throws IOException {
            return two(10, c, nat);
        }

        int dynamic(int bsm, int nat) throws IOException {
            return two(17, bsm, nat);
        }

        int indy(int bsm, int nat) throws IOException {
            return two(18, bsm, nat);
        }

        int mh(int kind, int ref) throws IOException {
            cp.writeByte(15);
            cp.writeByte(kind);
            cp.writeShort(ref);
            return next++;
        }

        int one(int tag, int a) throws IOException {
            cp.writeByte(tag);
            cp.writeShort(a);
            return next++;
        }

        byte[] build() throws IOException {
            ByteArrayOutputStream bytes = new ByteArrayOutputStream();
            DataOutputStream out = new DataOutputStream(bytes);
            out.writeInt(0xCAFEBABE);
            out.writeShort(0);
            out.writeShort(major);
            out.writeShort(next);
            cp.flush();
            out.write(cpBytes.toByteArray());
            out.writeShort(flags);
            out.writeShort(thisIdx);
            out.writeShort(superIdx);
            out.writeShort(0);
            out.writeShort(0);
            out.writeShort(methods.size());
            for (byte[] m : methods) {
                out.write(m);
            }
            out.writeShort(attrs.size());
            for (byte[] a : attrs) {
                out.write(a);
            }
            return bytes.toByteArray();
        }
    }

    static byte[] u2(int... v) {
        byte[] r = new byte[v.length * 2];
        for (int i = 0; i < v.length; i++) {
            r[2 * i] = (byte) (v[i] >> 8);
            r[2 * i + 1] = (byte) v[i];
        }
        return r;
    }

    static byte[] b(int... v) {
        byte[] r = new byte[v.length];
        for (int i = 0; i < v.length; i++) {
            r[i] = (byte) v[i];
        }
        return r;
    }

    static byte[] cat(byte[]... parts) {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        for (byte[] p : parts) {
            out.write(p, 0, p.length);
        }
        return out.toByteArray();
    }

    static byte[] attr(int nameIdx, byte[] body) throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeShort(nameIdx);
        out.writeInt(body.length);
        out.write(body);
        return bytes.toByteArray();
    }

    static byte[] member(int flags, int nameIdx, int descIdx, byte[]... attributes) throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeShort(flags);
        out.writeShort(nameIdx);
        out.writeShort(descIdx);
        out.writeShort(attributes.length);
        for (byte[] a : attributes) {
            out.write(a);
        }
        return bytes.toByteArray();
    }

    /** `public static m()V { return; }` with `maxLocals` and nested Code attributes. */
    static void addM(K k, int maxLocals, byte[]... nested) throws IOException {
        int codeU = k.utf8("Code");
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeShort(0);
        out.writeShort(maxLocals);
        out.writeInt(1);
        out.writeByte(0xb1);
        out.writeShort(0);
        out.writeShort(nested.length);
        for (byte[] n : nested) {
            out.write(n);
        }
        k.methods.add(member(0x0009, k.utf8("m"), k.utf8("()V"), attr(codeU, bytes.toByteArray())));
    }

    static void addM(K k) throws IOException {
        addM(k, 0);
    }

    /** 5 "m", 6 "()V", 7 NameAndType, 8 Methodref this.m()V; then `mh(kind, 8)` at 9. */
    static int methodrefToM(K k) throws IOException {
        int n = k.utf8("m");
        int d = k.utf8("()V");
        return k.methodref(k.thisIdx, k.nat(n, d));
    }

    /** The bootstrap rows' pool: MH at 9, indy(bsm) at 13, "BootstrapMethods" at 14. */
    static int bsmPool(K k, int bsmIndex) throws IOException {
        int mh = k.mh(6, methodrefToM(k)); // 9
        int run = k.nat(k.utf8("run"), k.utf8("()Ljava/lang/Runnable;")); // 12
        k.indy(bsmIndex, run); // 13
        int name = k.utf8("BootstrapMethods"); // 14
        if (mh != 9 || name != 14) {
            throw new AssertionError("pool layout " + mh + "/" + name);
        }
        return name;
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L5W110ClassFileParserMore.class.getClassLoader());
        }

        Class<?> def(String name, byte[] bytes) {
            return defineClass(name, bytes, 0, bytes.length);
        }
    }

    interface Body {
        byte[] make() throws IOException;
    }

    static void row(String label, String name, Body body) {
        String out;
        try {
            byte[] bytes = body.make();
            out = "defined " + new Loader().def(name, bytes).getName();
        } catch (Throwable t) {
            out = String.valueOf(t).split("\n")[0];
        }
        System.out.println(label + " -> " + out);
    }

    public static void main(String[] args) throws Exception {
        row("ok-indy", "L5W110Ok", () -> {
            K k = new K("L5W110Ok");
            int bsm = bsmPool(k, 0);
            addM(k);
            k.attrs.add(attr(bsm, u2(1, 9, 0)));
            return k.build();
        });

        // --- constant tags a version does not have (parse_constant_pool_entries) ---
        row("tag-mh-old", "L5W110To", () -> {
            K k = new K("L5W110To", 50);
            k.mh(6, methodrefToM(k));
            addM(k);
            return k.build();
        });
        row("tag-condy-old", "L5W110Tc", () -> {
            K k = new K("L5W110Tc", 54);
            k.mh(6, methodrefToM(k));
            k.dynamic(0, k.nat(k.utf8("x"), k.utf8("I")));
            addM(k);
            return k.build();
        });
        row("tag-module-old", "L5W110Tm", () -> {
            K k = new K("L5W110Tm", 52);
            k.one(19, k.utf8("mod"));
            addM(k);
            return k.build();
        });
        row("package-tag-in-class", "L5W110Tp", () -> {
            K k = new K("L5W110Tp");
            k.one(20, k.utf8("p"));
            addM(k);
            return k.build();
        });

        // --- the pool's index pass ---
        row("fieldref-class", "L5W110Fc", () -> {
            K k = new K("L5W110Fc");
            k.fieldref(1, k.nat(k.utf8("f"), k.utf8("I")));
            addM(k);
            return k.build();
        });
        row("methodref-nat", "L5W110Mt", () -> {
            K k = new K("L5W110Mt");
            k.methodref(k.thisIdx, 2);
            addM(k);
            return k.build();
        });
        row("nat-name", "L5W110Nn", () -> {
            K k = new K("L5W110Nn");
            k.nat(2, k.utf8("I"));
            addM(k);
            return k.build();
        });
        row("methodtype-desc", "L5W110Md", () -> {
            K k = new K("L5W110Md");
            k.one(16, 2);
            addM(k);
            return k.build();
        });
        row("indy-nat", "L5W110In", () -> {
            K k = new K("L5W110In");
            k.indy(0, 2);
            addM(k);
            return k.build();
        });
        row("mh-kind", "L5W110Hk", () -> {
            K k = new K("L5W110Hk");
            k.mh(10, methodrefToM(k)); // MethodHandle at 9 naming the Methodref at 8
            addM(k);
            return k.build();
        });
        row("mh-not-field", "L5W110Hf", () -> {
            K k = new K("L5W110Hf");
            k.mh(1, methodrefToM(k));
            addM(k);
            return k.build();
        });
        row("mh-not-interface", "L5W110Hi", () -> {
            K k = new K("L5W110Hi");
            k.mh(9, methodrefToM(k));
            addM(k);
            return k.build();
        });

        // --- the pool's format pass ---
        row("nat-zero-length", "L5W110Nz", () -> {
            K k = new K("L5W110Nz");
            k.nat(k.utf8("f"), k.utf8("")); // 5, 6, 7
            addM(k);
            return k.build();
        });
        row("nat-method-name", "L5W110Nm", () -> {
            K k = new K("L5W110Nm");
            k.nat(k.utf8("a.b"), k.utf8("()V"));
            addM(k);
            return k.build();
        });
        row("nat-field-sig", "L5W110Ns", () -> {
            K k = new K("L5W110Ns");
            k.nat(k.utf8("f"), k.utf8("Q"));
            addM(k);
            return k.build();
        });
        row("fieldref-method-sig", "L5W110Fm", () -> {
            K k = new K("L5W110Fm");
            k.fieldref(k.thisIdx, k.nat(k.utf8("f"), k.utf8("()V")));
            addM(k);
            return k.build();
        });
        row("methodref-field-sig", "L5W110Mf", () -> {
            K k = new K("L5W110Mf");
            k.methodref(k.thisIdx, k.nat(k.utf8("m2"), k.utf8("I")));
            addM(k);
            return k.build();
        });

        // --- SourceFile / EnclosingMethod (parse_classfile_attributes) ---
        row("sourcefile-twice", "L5W110St", () -> {
            K k = new K("L5W110St");
            int sf = k.utf8("SourceFile");
            int file = k.utf8("X.java");
            addM(k);
            k.attrs.add(attr(sf, u2(file)));
            k.attrs.add(attr(sf, u2(file)));
            return k.build();
        });
        row("sourcefile-index", "L5W110Si", () -> {
            K k = new K("L5W110Si");
            int sf = k.utf8("SourceFile");
            addM(k);
            k.attrs.add(attr(sf, u2(2)));
            return k.build();
        });
        row("sourcefile-length", "L5W110Sl", () -> {
            K k = new K("L5W110Sl");
            int sf = k.utf8("SourceFile");
            int file = k.utf8("X.java");
            addM(k);
            k.attrs.add(attr(sf, cat(u2(file), b(0))));
            return k.build();
        });
        row("enclosing-twice", "L5W110Et", () -> {
            K k = new K("L5W110Et");
            int em = k.utf8("EnclosingMethod");
            addM(k);
            k.attrs.add(attr(em, u2(4, 0)));
            k.attrs.add(attr(em, u2(4, 0)));
            return k.build();
        });
        row("enclosing-class-0", "L5W110E0", () -> {
            K k = new K("L5W110E0");
            int em = k.utf8("EnclosingMethod");
            addM(k);
            k.attrs.add(attr(em, u2(0, 0)));
            return k.build();
        });
        row("enclosing-class-bad", "L5W110Ec", () -> {
            K k = new K("L5W110Ec");
            int em = k.utf8("EnclosingMethod");
            addM(k);
            k.attrs.add(attr(em, u2(1, 0)));
            return k.build();
        });
        row("enclosing-method-bad", "L5W110Em", () -> {
            K k = new K("L5W110Em");
            int em = k.utf8("EnclosingMethod");
            addM(k);
            k.attrs.add(attr(em, u2(4, 2)));
            return k.build();
        });

        // --- BootstrapMethods ---
        row("bsm-missing", "L5W110Bm", () -> {
            K k = new K("L5W110Bm");
            bsmPool(k, 0);
            addM(k);
            return k.build();
        });
        row("bsm-short", "L5W110Bs", () -> {
            K k = new K("L5W110Bs");
            int bsm = bsmPool(k, 1);
            addM(k);
            k.attrs.add(attr(bsm, u2(1, 9, 0)));
            return k.build();
        });
        row("bsm-twice", "L5W110Bt", () -> {
            K k = new K("L5W110Bt");
            int bsm = bsmPool(k, 0);
            addM(k);
            k.attrs.add(attr(bsm, u2(1, 9, 0)));
            k.attrs.add(attr(bsm, u2(1, 9, 0)));
            return k.build();
        });
        row("bsm-method-type", "L5W110Bh", () -> {
            K k = new K("L5W110Bh");
            int bsm = bsmPool(k, 0);
            addM(k);
            k.attrs.add(attr(bsm, u2(1, 2, 0)));
            return k.build();
        });
        row("bsm-arg", "L5W110Ba", () -> {
            K k = new K("L5W110Ba");
            int bsm = bsmPool(k, 0);
            addM(k);
            k.attrs.add(attr(bsm, u2(1, 9, 1, 1)));
            return k.build();
        });

        // --- a method's Exceptions attribute ---
        row("exceptions-twice", "L5W110Xt", () -> {
            K k = new K("L5W110Xt");
            int ex = k.utf8("Exceptions");
            int exc = k.cls(k.utf8("java/lang/Exception"));
            k.methods.add(member(0x0109, k.utf8("n"), k.utf8("()V"),
                attr(ex, u2(1, exc)), attr(ex, u2(1, exc))));
            return k.build();
        });
        row("exceptions-type", "L5W110Xb", () -> {
            K k = new K("L5W110Xb");
            int ex = k.utf8("Exceptions");
            k.methods.add(member(0x0109, k.utf8("n"), k.utf8("()V"), attr(ex, u2(1, 1))));
            return k.build();
        });
        row("exceptions-length", "L5W110Xl", () -> {
            K k = new K("L5W110Xl");
            int ex = k.utf8("Exceptions");
            k.methods.add(member(0x0109, k.utf8("n"), k.utf8("()V"), attr(ex, u2(0, 0))));
            return k.build();
        });

        // --- Code's own attributes ---
        row("code-attr-name", "L5W110Ca", () -> {
            K k = new K("L5W110Ca");
            addM(k, 0, attr(2, new byte[0]));
            return k.build();
        });
        row("lvt-dup", "L5W110Ld", () -> {
            K k = new K("L5W110Ld");
            int lvt = k.utf8("LocalVariableTable");
            int x = k.utf8("x");
            int i = k.utf8("I");
            addM(k, 1, attr(lvt, u2(2, 0, 1, x, i, 0, 0, 1, x, i, 0)));
            return k.build();
        });
        row("lvtt-unmatched", "L5W110Lu", () -> {
            K k = new K("L5W110Lu");
            int lvt = k.utf8("LocalVariableTable");
            int lvtt = k.utf8("LocalVariableTypeTable");
            int x = k.utf8("x");
            int y = k.utf8("y");
            int i = k.utf8("I");
            addM(k, 1, attr(lvt, u2(1, 0, 1, x, i, 0)), attr(lvtt, u2(1, 0, 1, y, i, 0)));
            return k.build();
        });
        row("lvtt-dup", "L5W110Lt", () -> {
            K k = new K("L5W110Lt");
            int lvt = k.utf8("LocalVariableTable");
            int lvtt = k.utf8("LocalVariableTypeTable");
            int x = k.utf8("x");
            int i = k.utf8("I");
            addM(k, 1, attr(lvt, u2(1, 0, 1, x, i, 0)),
                attr(lvtt, u2(2, 0, 1, x, i, 0, 0, 1, x, i, 0)));
            return k.build();
        });
    }
}
