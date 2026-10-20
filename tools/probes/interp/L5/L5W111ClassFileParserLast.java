// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, wave 111, lane L5 (page
// interpreter-i109-L5-classfileparser-checks-not-made-FIXED-20261017, the close-out):
//
//   ignored-*  attributes HotSpot 25's `ClassFileParser` SKIPS in that table
//              or class version, with bodies the reader cannot decode. HotSpot
//              defines each class; CratonVM refused them (the reader
//              shape-checked and decoded every attribute by name wherever it
//              stood) -- the direction that breaks applications.
//   the rest   refusals HotSpot makes and CratonVM did not (or made in its own
//              words): MethodType descriptors, MethodHandle target names,
//              Signature lengths / index / repeats, repeated
//              SourceDebugExtension / InnerClasses / annotation /
//              MethodParameters / Code attributes, Code on a native method, a
//              missing Code, the BootstrapMethods length texts.
//
// Each class is hand-assembled from raw bytes (`public class X extends Object`,
// version 69 unless the row says otherwise) with exactly one defect, defined
// by a fresh loader under its binary name; the row prints
// `Throwable.toString()` (or `defined X`).
//
// Run (no setup):
//   javac -d out L5W111ClassFileParserLast.java
//   java -cp out L5W111ClassFileParserLast                             (HotSpot 25)
//   cratonvm --jdk-only   [--nojit] -cp out L5W111ClassFileParserLast
//   cratonvm --compatible [--nojit] -cp out L5W111ClassFileParserLast
//
// HOTSPOT_EXPECTED predicted from the JDK 25 sources (classFileParser.cpp), not run.
// HOTSPOT_EXPECTED_BEGIN
// ignored-nesthost-old -> defined L5W111Ia
// ignored-constantvalue-instance -> defined L5W111Ib
// ignored-sourcefile-in-method -> defined L5W111Ic
// ignored-modulemainclass -> defined L5W111Id
// ignored-annotations-malformed -> defined L5W111Ie
// ignored-in-code -> defined L5W111If
// ignored-lnt-in-method -> defined L5W111Ig
// ignored-bootstrap-old -> defined L5W111Ih
// ignored-record-old -> defined L5W111Ii
// methodtype-sig -> java.lang.ClassFormatError: Method "type" in class L5W111Mt has illegal signature "(I"
// mh-new-not-init -> java.lang.ClassFormatError: Bad constructor name at constant pool index 5 in class file L5W111Mn
// mh-invoke-init -> java.lang.ClassFormatError: Bad method name at constant pool index 5 in class file L5W111Mi
// signature-class-length -> java.lang.ClassFormatError: Wrong Signature attribute length 3 in class file L5W111Sa
// signature-field-length -> java.lang.ClassFormatError: Wrong size 3 for field's Signature attribute in class file L5W111Sb
// signature-method-length -> java.lang.ClassFormatError: Invalid Signature attribute length 3 in class file L5W111Sc
// signature-index -> java.lang.ClassFormatError: Invalid constant pool index 2 in Signature attribute in class file L5W111Sd
// signature-twice-class -> java.lang.ClassFormatError: Multiple Signature attributes in class file L5W111Se
// signature-twice-field -> java.lang.ClassFormatError: Multiple Signature attributes for field in class file L5W111Sf
// signature-twice-method -> java.lang.ClassFormatError: Multiple Signature attributes for method in class file L5W111Sg
// sde-twice -> java.lang.ClassFormatError: Multiple SourceDebugExtension attributes in class file L5W111Da
// innerclasses-twice -> java.lang.ClassFormatError: Multiple InnerClasses attributes in class file L5W111Db
// rva-twice-class -> java.lang.ClassFormatError: Multiple RuntimeVisibleAnnotations attributes in class file L5W111Ra
// rva-twice-field -> java.lang.ClassFormatError: Multiple RuntimeVisibleAnnotations attributes for field in class file L5W111Rb
// rva-twice-method -> java.lang.ClassFormatError: Multiple RuntimeVisibleAnnotations attributes for method in class file L5W111Rc
// rvpa-twice -> java.lang.ClassFormatError: Multiple RuntimeVisibleParameterAnnotations attributes for method in class file L5W111Rd
// annotation-default-twice -> java.lang.ClassFormatError: Multiple AnnotationDefault attributes for method in class file L5W111Re
// rvta-twice-method -> java.lang.ClassFormatError: Multiple RuntimeVisibleTypeAnnotations attributes for method in class file L5W111Rf
// methodparameters-twice -> java.lang.ClassFormatError: Multiple MethodParameters attributes in class file L5W111Pa
// code-twice -> java.lang.ClassFormatError: Multiple Code attributes in class file L5W111Ca
// code-in-native -> java.lang.ClassFormatError: Code attribute in native or abstract methods in class file L5W111Cb
// code-absent -> java.lang.ClassFormatError: Absent Code attribute in method that is not native or abstract in class file L5W111Cc
// bsm-length-short -> java.lang.ClassFormatError: Invalid BootstrapMethods attribute length 1 in class file L5W111Ba
// bsm-overrun -> java.lang.ClassFormatError: Invalid BootstrapMethods num_bootstrap_methods or num_bootstrap_arguments value in class file L5W111Bb
// bsm-trailing -> java.lang.ClassFormatError: Bad length on BootstrapMethods in class file L5W111Bc
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.util.ArrayList;
import java.util.List;

public class L5W111ClassFileParserLast {

    /** Raw class-file writer; constant-pool entries are appended in call order, never shared. */
    static final class K {
        final ByteArrayOutputStream cpBytes = new ByteArrayOutputStream();
        final DataOutputStream cp = new DataOutputStream(cpBytes);
        int next = 1;
        final int major;
        int flags = 0x21;
        final int thisIdx;
        final int superIdx;
        final List<byte[]> fields = new ArrayList<>();
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

        int methodref(int c, int nat) throws IOException {
            return two(10, c, nat);
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
            out.writeShort(fields.size());
            for (byte[] f : fields) {
                out.write(f);
            }
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

    /** A `Code` body `return` with `maxLocals` and nested attributes. */
    static byte[] codeBody(int maxLocals, byte[]... nested) throws IOException {
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
        return bytes.toByteArray();
    }

    /** `public static m()V { return; }`, plus `extra` method attributes after its Code. */
    static void addM(K k, byte[]... extra) throws IOException {
        int codeU = k.utf8("Code");
        byte[][] all = new byte[extra.length + 1][];
        all[0] = attr(codeU, codeBody(0));
        System.arraycopy(extra, 0, all, 1, extra.length);
        k.methods.add(member(0x0009, k.utf8("m"), k.utf8("()V"), all));
    }

    /** `public static native n()V` with `attributes`. */
    static void addNative(K k, byte[]... attributes) throws IOException {
        k.methods.add(member(0x0109, k.utf8("n"), k.utf8("()V"), attributes));
    }

    /** 5 "m" (or `name`), 6 "()V", 7 NameAndType, 8 Methodref this.<name>()V. */
    static int methodrefTo(K k, String name) throws IOException {
        int n = k.utf8(name);
        int d = k.utf8("()V");
        return k.methodref(k.thisIdx, k.nat(n, d));
    }

    /** MH invokeStatic at 9, indy(0) at 13, "BootstrapMethods" at 14. */
    static int bsmPool(K k) throws IOException {
        int mh = k.mh(6, methodrefTo(k, "m"));
        int run = k.nat(k.utf8("run"), k.utf8("()Ljava/lang/Runnable;"));
        k.indy(0, run);
        int name = k.utf8("BootstrapMethods");
        if (mh != 9 || name != 14) {
            throw new AssertionError("pool layout " + mh + "/" + name);
        }
        return name;
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L5W111ClassFileParserLast.class.getClassLoader());
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
        // --- attributes HotSpot skips: the classes are defined ---
        row("ignored-nesthost-old", "L5W111Ia", () -> {
            K k = new K("L5W111Ia", 52);
            int nh = k.utf8("NestHost");
            addM(k);
            k.attrs.add(attr(nh, b(0, 4, 0))); // 3 bytes; NestHost is version 55+
            return k.build();
        });
        row("ignored-constantvalue-instance", "L5W111Ib", () -> {
            K k = new K("L5W111Ib");
            int cv = k.utf8("ConstantValue");
            k.fields.add(member(0x0001, k.utf8("f"), k.utf8("I"), attr(cv, b(0, 1, 0))));
            addM(k);
            return k.build();
        });
        row("ignored-sourcefile-in-method", "L5W111Ic", () -> {
            K k = new K("L5W111Ic");
            int sf = k.utf8("SourceFile");
            addM(k, attr(sf, b(0, 1, 0)));
            return k.build();
        });
        row("ignored-modulemainclass", "L5W111Id", () -> {
            K k = new K("L5W111Id");
            int mm = k.utf8("ModuleMainClass");
            addM(k);
            k.attrs.add(attr(mm, b(0, 2, 0)));
            return k.build();
        });
        row("ignored-annotations-malformed", "L5W111Ie", () -> {
            K k = new K("L5W111Ie");
            int rva = k.utf8("RuntimeVisibleAnnotations");
            addM(k, attr(rva, u2(1))); // one annotation, no bytes for it
            return k.build();
        });
        row("ignored-in-code", "L5W111If", () -> {
            K k = new K("L5W111If");
            int sf = k.utf8("SourceFile");
            int codeU = k.utf8("Code");
            k.methods.add(member(0x0009, k.utf8("m"), k.utf8("()V"),
                attr(codeU, codeBody(0, attr(sf, b(0, 1, 0))))));
            return k.build();
        });
        row("ignored-lnt-in-method", "L5W111Ig", () -> {
            K k = new K("L5W111Ig");
            int lnt = k.utf8("LineNumberTable");
            addM(k, attr(lnt, u2(5))); // five entries, no bytes for them
            return k.build();
        });
        row("ignored-bootstrap-old", "L5W111Ih", () -> {
            K k = new K("L5W111Ih", 50);
            int bsm = k.utf8("BootstrapMethods");
            addM(k);
            k.attrs.add(attr(bsm, b(7))); // BootstrapMethods is version 51+
            return k.build();
        });
        row("ignored-record-old", "L5W111Ii", () -> {
            K k = new K("L5W111Ii", 55);
            int rec = k.utf8("Record");
            addM(k);
            k.attrs.add(attr(rec, b(9))); // Record is version 60+
            return k.build();
        });

        // --- the pool's format pass ---
        row("methodtype-sig", "L5W111Mt", () -> {
            K k = new K("L5W111Mt");
            k.one(16, k.utf8("(I"));
            addM(k);
            return k.build();
        });
        row("mh-new-not-init", "L5W111Mn", () -> {
            K k = new K("L5W111Mn");
            k.mh(8, methodrefTo(k, "m"));
            addM(k);
            return k.build();
        });
        row("mh-invoke-init", "L5W111Mi", () -> {
            K k = new K("L5W111Mi");
            k.mh(5, methodrefTo(k, "<init>"));
            addM(k);
            return k.build();
        });

        // --- Signature ---
        row("signature-class-length", "L5W111Sa", () -> {
            K k = new K("L5W111Sa");
            int sig = k.utf8("Signature");
            int s = k.utf8("Ljava/lang/Object;");
            addM(k);
            k.attrs.add(attr(sig, cat(u2(s), b(0))));
            return k.build();
        });
        row("signature-field-length", "L5W111Sb", () -> {
            K k = new K("L5W111Sb");
            int sig = k.utf8("Signature");
            int s = k.utf8("I");
            k.fields.add(member(0x0009, k.utf8("f"), k.utf8("I"), attr(sig, cat(u2(s), b(0)))));
            addM(k);
            return k.build();
        });
        row("signature-method-length", "L5W111Sc", () -> {
            K k = new K("L5W111Sc");
            int sig = k.utf8("Signature");
            int s = k.utf8("()V");
            addM(k, attr(sig, cat(u2(s), b(0))));
            return k.build();
        });
        row("signature-index", "L5W111Sd", () -> {
            K k = new K("L5W111Sd");
            int sig = k.utf8("Signature");
            addM(k);
            k.attrs.add(attr(sig, u2(2)));
            return k.build();
        });
        row("signature-twice-class", "L5W111Se", () -> {
            K k = new K("L5W111Se");
            int sig = k.utf8("Signature");
            int s = k.utf8("Ljava/lang/Object;");
            addM(k);
            k.attrs.add(attr(sig, u2(s)));
            k.attrs.add(attr(sig, u2(s)));
            return k.build();
        });
        row("signature-twice-field", "L5W111Sf", () -> {
            K k = new K("L5W111Sf");
            int sig = k.utf8("Signature");
            int s = k.utf8("I");
            k.fields.add(member(0x0009, k.utf8("f"), k.utf8("I"), attr(sig, u2(s)), attr(sig, u2(s))));
            addM(k);
            return k.build();
        });
        row("signature-twice-method", "L5W111Sg", () -> {
            K k = new K("L5W111Sg");
            int sig = k.utf8("Signature");
            int s = k.utf8("()V");
            addM(k, attr(sig, u2(s)), attr(sig, u2(s)));
            return k.build();
        });

        // --- repeated class / member attributes ---
        row("sde-twice", "L5W111Da", () -> {
            K k = new K("L5W111Da");
            int sde = k.utf8("SourceDebugExtension");
            addM(k);
            k.attrs.add(attr(sde, b('x')));
            k.attrs.add(attr(sde, b('y')));
            return k.build();
        });
        row("innerclasses-twice", "L5W111Db", () -> {
            K k = new K("L5W111Db");
            int ic = k.utf8("InnerClasses");
            addM(k);
            k.attrs.add(attr(ic, u2(0)));
            k.attrs.add(attr(ic, u2(0)));
            return k.build();
        });
        row("rva-twice-class", "L5W111Ra", () -> {
            K k = new K("L5W111Ra");
            int rva = k.utf8("RuntimeVisibleAnnotations");
            addM(k);
            k.attrs.add(attr(rva, u2(0)));
            k.attrs.add(attr(rva, u2(0)));
            return k.build();
        });
        row("rva-twice-field", "L5W111Rb", () -> {
            K k = new K("L5W111Rb");
            int rva = k.utf8("RuntimeVisibleAnnotations");
            k.fields.add(member(0x0009, k.utf8("f"), k.utf8("I"), attr(rva, u2(0)), attr(rva, u2(0))));
            addM(k);
            return k.build();
        });
        row("rva-twice-method", "L5W111Rc", () -> {
            K k = new K("L5W111Rc");
            int rva = k.utf8("RuntimeVisibleAnnotations");
            addM(k, attr(rva, u2(0)), attr(rva, u2(0)));
            return k.build();
        });
        row("rvpa-twice", "L5W111Rd", () -> {
            K k = new K("L5W111Rd");
            int rvpa = k.utf8("RuntimeVisibleParameterAnnotations");
            addM(k, attr(rvpa, b(0)), attr(rvpa, b(0)));
            return k.build();
        });
        row("annotation-default-twice", "L5W111Re", () -> {
            K k = new K("L5W111Re");
            int ad = k.utf8("AnnotationDefault");
            int v = k.utf8("v");
            byte[] value = cat(b('s'), u2(v));
            addNative(k, attr(ad, value), attr(ad, value));
            return k.build();
        });
        row("rvta-twice-method", "L5W111Rf", () -> {
            K k = new K("L5W111Rf");
            int rvta = k.utf8("RuntimeVisibleTypeAnnotations");
            addM(k, attr(rvta, u2(0)), attr(rvta, u2(0)));
            return k.build();
        });
        row("methodparameters-twice", "L5W111Pa", () -> {
            K k = new K("L5W111Pa");
            int mp = k.utf8("MethodParameters");
            addM(k, attr(mp, b(0)), attr(mp, b(0)));
            return k.build();
        });

        // --- Code presence ---
        row("code-twice", "L5W111Ca", () -> {
            K k = new K("L5W111Ca");
            int codeU = k.utf8("Code");
            addM(k, attr(codeU, codeBody(0)));
            return k.build();
        });
        row("code-in-native", "L5W111Cb", () -> {
            K k = new K("L5W111Cb");
            int codeU = k.utf8("Code");
            addNative(k, attr(codeU, codeBody(0)));
            return k.build();
        });
        row("code-absent", "L5W111Cc", () -> {
            K k = new K("L5W111Cc");
            k.methods.add(member(0x0009, k.utf8("m"), k.utf8("()V")));
            return k.build();
        });

        // --- BootstrapMethods lengths ---
        row("bsm-length-short", "L5W111Ba", () -> {
            K k = new K("L5W111Ba");
            int bsm = k.utf8("BootstrapMethods");
            addM(k);
            k.attrs.add(attr(bsm, b(0)));
            return k.build();
        });
        row("bsm-overrun", "L5W111Bb", () -> {
            K k = new K("L5W111Bb");
            int bsm = bsmPool(k);
            addM(k);
            k.attrs.add(attr(bsm, u2(1, 9, 5)));
            return k.build();
        });
        row("bsm-trailing", "L5W111Bc", () -> {
            K k = new K("L5W111Bc");
            int bsm = bsmPool(k);
            addM(k);
            k.attrs.add(attr(bsm, u2(1, 9, 0, 0)));
            return k.build();
        });
    }
}
