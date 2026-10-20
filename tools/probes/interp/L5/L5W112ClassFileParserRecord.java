// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, wave 112, lane L5 (page
// interpreter-i109-L5-classfileparser-checks-not-made-FIXED-20261017, the last items): HotSpot
// 25's `ClassFileParser` on the Record attribute (`parse_classfile_record_attribute`,
// read only for a class whose superclass is java/lang/Record), Synthetic /
// Deprecated lengths, a Code attribute's LineNumberTable / LocalVariable(Type)Table
// lengths and the version-gated LocalVariableTypeTable / StackMapTable, repeated
// invisible annotation attributes, and SourceDebugExtension (never checked by
// HotSpot). Each class is hand-assembled from raw bytes (version 69, `public
// class X extends Object` with one static method `m()V`, unless the row says
// otherwise) with exactly one defect, defined by a fresh loader under its
// binary name; the row prints `Throwable.toString()` (or `defined X`).
//
// Run (no setup):
//   javac -d out L5W112ClassFileParserRecord.java
//   java -cp out L5W112ClassFileParserRecord                             (HotSpot 25)
//   cratonvm --jdk-only   [--nojit] -cp out L5W112ClassFileParserRecord
//   cratonvm --compatible [--nojit] -cp out L5W112ClassFileParserRecord
//
// HOTSPOT_EXPECTED predicted from the JDK 25 sources (classFileParser.cpp), not run.
// HOTSPOT_EXPECTED_BEGIN
// record-ok -> defined L5W112Ra
// record-not-record-super -> java.lang.ClassFormatError: Truncated class file
// record-annotation-malformed -> defined L5W112Rc
// record-twice -> java.lang.ClassFormatError: Multiple Record attributes in class file L5W112Rd
// record-length -> java.lang.ClassFormatError: Record attribute has wrong length in class file L5W112Re
// record-name-index -> java.lang.ClassFormatError: Invalid constant pool index 2 for name in Record attribute in class file L5W112Rf
// record-name-illegal -> java.lang.ClassFormatError: Illegal field name "a.b" in class L5W112Rg
// record-descriptor-index -> java.lang.ClassFormatError: Invalid constant pool index 2 for descriptor in Record attribute in class file L5W112Rh
// record-descriptor-illegal -> java.lang.ClassFormatError: Field "x" in class L5W112Ri has illegal signature "Q"
// record-attr-name-index -> java.lang.ClassFormatError: Invalid Record attribute name index 2 in class file L5W112Rj
// record-signature-length -> java.lang.ClassFormatError: Invalid Signature attribute length 3 in Record component in class file L5W112Rk
// record-signature-twice -> java.lang.ClassFormatError: Multiple Signature attributes for Record component in class file L5W112Rl
// record-rva-twice -> java.lang.ClassFormatError: Multiple RuntimeVisibleAnnotations attributes for Record component in class file L5W112Rm
// record-ria-twice -> java.lang.ClassFormatError: Multiple RuntimeInvisibleAnnotations attributes for Record component in class file L5W112Rn
// synthetic-class-length -> java.lang.ClassFormatError: Invalid Synthetic classfile attribute length 1 in class file L5W112Sa
// deprecated-field-length -> java.lang.ClassFormatError: Invalid Deprecated field attribute length 1 in class file L5W112Sb
// synthetic-method-length -> java.lang.ClassFormatError: Invalid Synthetic method attribute length 1 in class file L5W112Sc
// lnt-wrong-length -> java.lang.ClassFormatError: LineNumberTable attribute has wrong length in class file L5W112La
// lvt-wrong-length -> java.lang.ClassFormatError: LocalVariableTable has wrong length in class file L5W112Lb
// lvtt-wrong-length -> java.lang.ClassFormatError: LocalVariableTypeTable has wrong length in class file L5W112Lc
// lvtt-old-skipped -> defined L5W112Ld
// stackmap-twice-old -> defined L5W112Le
// stackmap-twice -> java.lang.ClassFormatError: Multiple StackMapTable attributes in class file L5W112Lf
// ria-twice-class -> java.lang.ClassFormatError: Multiple RuntimeInvisibleAnnotations attributes in class file L5W112Ia
// ria-twice-field -> java.lang.ClassFormatError: Multiple RuntimeInvisibleAnnotations attributes for field in class file L5W112Ib
// ripa-twice-method -> java.lang.ClassFormatError: Multiple RuntimeInvisibleParameterAnnotations attributes for method in class file L5W112Ic
// rita-twice-method -> java.lang.ClassFormatError: Multiple RuntimeInvisibleTypeAnnotations attributes for method in class file L5W112Id
// sde-not-utf8 -> defined L5W112Da
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.util.ArrayList;
import java.util.List;

public class L5W112ClassFileParserRecord {

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

        /** 1 Utf8 name, 2 its Class, 3 Utf8 super, 4 its Class. */
        K(String name, String superName, int major) throws IOException {
            this.major = major;
            thisIdx = cls(utf8(name));
            superIdx = cls(utf8(superName));
        }

        K(String name) throws IOException {
            this(name, "java/lang/Object", 69);
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

    /** `public static m()V { return; }` with `maxLocals`, nested Code attributes, then `extra` method attributes. */
    static void addM(K k, int maxLocals, byte[][] nested, byte[]... extra) throws IOException {
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
        byte[][] all = new byte[extra.length + 1][];
        all[0] = attr(codeU, bytes.toByteArray());
        System.arraycopy(extra, 0, all, 1, extra.length);
        k.methods.add(member(0x0009, k.utf8("m"), k.utf8("()V"), all));
    }

    static void addM(K k) throws IOException {
        addM(k, 0, new byte[0][]);
    }

    /** A record class (`final`, superclass java/lang/Record). */
    static K record(String name) throws IOException {
        K k = new K(name, "java/lang/Record", 69);
        k.flags = 0x31;
        return k;
    }

    /** One record component `name`:`desc` with `attributes`. */
    static byte[] component(int name, int desc, byte[]... attributes) {
        byte[] out = cat(u2(name, desc, attributes.length));
        for (byte[] a : attributes) {
            out = cat(out, a);
        }
        return out;
    }

    static byte[] recordBody(byte[]... components) {
        byte[] out = u2(components.length);
        for (byte[] c : components) {
            out = cat(out, c);
        }
        return out;
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L5W112ClassFileParserRecord.class.getClassLoader());
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
        // --- Record ---
        row("record-ok", "L5W112Ra", () -> {
            K k = record("L5W112Ra");
            int rec = k.utf8("Record");
            int x = k.utf8("x");
            int i = k.utf8("I");
            addM(k);
            k.attrs.add(attr(rec, recordBody(component(x, i))));
            return k.build();
        });
        row("record-not-record-super", "L5W112Rb", () -> {
            K k = new K("L5W112Rb");
            int rec = k.utf8("Record");
            addM(k);
            k.attrs.add(attr(rec, b(9))); // skipped: the superclass is not java/lang/Record
            return k.build();
        });
        row("record-annotation-malformed", "L5W112Rc", () -> {
            K k = record("L5W112Rc");
            int rec = k.utf8("Record");
            int rva = k.utf8("RuntimeVisibleAnnotations");
            int x = k.utf8("x");
            int i = k.utf8("I");
            addM(k);
            k.attrs.add(attr(rec, recordBody(component(x, i, attr(rva, u2(1))))));
            return k.build();
        });
        row("record-twice", "L5W112Rd", () -> {
            K k = record("L5W112Rd");
            int rec = k.utf8("Record");
            int x = k.utf8("x");
            int i = k.utf8("I");
            addM(k);
            k.attrs.add(attr(rec, recordBody(component(x, i))));
            k.attrs.add(attr(rec, recordBody(component(x, i))));
            return k.build();
        });
        row("record-length", "L5W112Re", () -> {
            K k = record("L5W112Re");
            int rec = k.utf8("Record");
            int x = k.utf8("x");
            int i = k.utf8("I");
            addM(k);
            k.attrs.add(attr(rec, cat(recordBody(component(x, i)), b(0))));
            return k.build();
        });
        row("record-name-index", "L5W112Rf", () -> {
            K k = record("L5W112Rf");
            int rec = k.utf8("Record");
            int i = k.utf8("I");
            addM(k);
            k.attrs.add(attr(rec, recordBody(component(2, i))));
            return k.build();
        });
        row("record-name-illegal", "L5W112Rg", () -> {
            K k = record("L5W112Rg");
            int rec = k.utf8("Record");
            int x = k.utf8("a.b");
            int i = k.utf8("I");
            addM(k);
            k.attrs.add(attr(rec, recordBody(component(x, i))));
            return k.build();
        });
        row("record-descriptor-index", "L5W112Rh", () -> {
            K k = record("L5W112Rh");
            int rec = k.utf8("Record");
            int x = k.utf8("x");
            addM(k);
            k.attrs.add(attr(rec, recordBody(component(x, 2))));
            return k.build();
        });
        row("record-descriptor-illegal", "L5W112Ri", () -> {
            K k = record("L5W112Ri");
            int rec = k.utf8("Record");
            int x = k.utf8("x");
            int q = k.utf8("Q");
            addM(k);
            k.attrs.add(attr(rec, recordBody(component(x, q))));
            return k.build();
        });
        row("record-attr-name-index", "L5W112Rj", () -> {
            K k = record("L5W112Rj");
            int rec = k.utf8("Record");
            int x = k.utf8("x");
            int i = k.utf8("I");
            addM(k);
            k.attrs.add(attr(rec, recordBody(component(x, i, attr(2, new byte[0])))));
            return k.build();
        });
        row("record-signature-length", "L5W112Rk", () -> {
            K k = record("L5W112Rk");
            int rec = k.utf8("Record");
            int sig = k.utf8("Signature");
            int x = k.utf8("x");
            int i = k.utf8("I");
            addM(k);
            k.attrs.add(attr(rec, recordBody(component(x, i, attr(sig, cat(u2(i), b(0)))))));
            return k.build();
        });
        row("record-signature-twice", "L5W112Rl", () -> {
            K k = record("L5W112Rl");
            int rec = k.utf8("Record");
            int sig = k.utf8("Signature");
            int x = k.utf8("x");
            int i = k.utf8("I");
            addM(k);
            k.attrs.add(attr(rec, recordBody(component(x, i, attr(sig, u2(i)), attr(sig, u2(i))))));
            return k.build();
        });
        row("record-rva-twice", "L5W112Rm", () -> {
            K k = record("L5W112Rm");
            int rec = k.utf8("Record");
            int rva = k.utf8("RuntimeVisibleAnnotations");
            int x = k.utf8("x");
            int i = k.utf8("I");
            addM(k);
            k.attrs.add(attr(rec, recordBody(component(x, i, attr(rva, u2(0)), attr(rva, u2(0))))));
            return k.build();
        });
        row("record-ria-twice", "L5W112Rn", () -> {
            K k = record("L5W112Rn");
            int rec = k.utf8("Record");
            int ria = k.utf8("RuntimeInvisibleAnnotations");
            int x = k.utf8("x");
            int i = k.utf8("I");
            addM(k);
            k.attrs.add(attr(rec, recordBody(component(x, i, attr(ria, u2(0)), attr(ria, u2(0))))));
            return k.build();
        });

        // --- Synthetic / Deprecated lengths ---
        row("synthetic-class-length", "L5W112Sa", () -> {
            K k = new K("L5W112Sa");
            int syn = k.utf8("Synthetic");
            addM(k);
            k.attrs.add(attr(syn, b(0)));
            return k.build();
        });
        row("deprecated-field-length", "L5W112Sb", () -> {
            K k = new K("L5W112Sb");
            int dep = k.utf8("Deprecated");
            k.fields.add(member(0x0009, k.utf8("f"), k.utf8("I"), attr(dep, b(0))));
            addM(k);
            return k.build();
        });
        row("synthetic-method-length", "L5W112Sc", () -> {
            K k = new K("L5W112Sc");
            int syn = k.utf8("Synthetic");
            addM(k, 0, new byte[0][], attr(syn, b(0)));
            return k.build();
        });

        // --- a Code attribute's tables ---
        row("lnt-wrong-length", "L5W112La", () -> {
            K k = new K("L5W112La");
            int lnt = k.utf8("LineNumberTable");
            addM(k, 0, new byte[][] {attr(lnt, u2(1))});
            return k.build();
        });
        row("lvt-wrong-length", "L5W112Lb", () -> {
            K k = new K("L5W112Lb");
            int lvt = k.utf8("LocalVariableTable");
            addM(k, 1, new byte[][] {attr(lvt, u2(1))});
            return k.build();
        });
        row("lvtt-wrong-length", "L5W112Lc", () -> {
            K k = new K("L5W112Lc");
            int lvtt = k.utf8("LocalVariableTypeTable");
            addM(k, 1, new byte[][] {attr(lvtt, u2(1))});
            return k.build();
        });
        row("lvtt-old-skipped", "L5W112Ld", () -> {
            K k = new K("L5W112Ld", "java/lang/Object", 48);
            int lvtt = k.utf8("LocalVariableTypeTable");
            addM(k, 1, new byte[][] {attr(lvtt, u2(1))});
            return k.build();
        });
        row("stackmap-twice-old", "L5W112Le", () -> {
            K k = new K("L5W112Le", "java/lang/Object", 49);
            int smt = k.utf8("StackMapTable");
            addM(k, 0, new byte[][] {attr(smt, u2(0)), attr(smt, u2(0))});
            return k.build();
        });
        row("stackmap-twice", "L5W112Lf", () -> {
            K k = new K("L5W112Lf");
            int smt = k.utf8("StackMapTable");
            addM(k, 0, new byte[][] {attr(smt, u2(0)), attr(smt, u2(0))});
            return k.build();
        });

        // --- repeated invisible annotations ---
        row("ria-twice-class", "L5W112Ia", () -> {
            K k = new K("L5W112Ia");
            int ria = k.utf8("RuntimeInvisibleAnnotations");
            addM(k);
            k.attrs.add(attr(ria, u2(0)));
            k.attrs.add(attr(ria, u2(0)));
            return k.build();
        });
        row("ria-twice-field", "L5W112Ib", () -> {
            K k = new K("L5W112Ib");
            int ria = k.utf8("RuntimeInvisibleAnnotations");
            k.fields.add(member(0x0009, k.utf8("f"), k.utf8("I"), attr(ria, u2(0)), attr(ria, u2(0))));
            addM(k);
            return k.build();
        });
        row("ripa-twice-method", "L5W112Ic", () -> {
            K k = new K("L5W112Ic");
            int ripa = k.utf8("RuntimeInvisibleParameterAnnotations");
            addM(k, 0, new byte[0][], attr(ripa, b(0)), attr(ripa, b(0)));
            return k.build();
        });
        row("rita-twice-method", "L5W112Id", () -> {
            K k = new K("L5W112Id");
            int rita = k.utf8("RuntimeInvisibleTypeAnnotations");
            addM(k, 0, new byte[0][], attr(rita, u2(0)), attr(rita, u2(0)));
            return k.build();
        });

        // --- SourceDebugExtension: HotSpot keeps the bytes as they are ---
        row("sde-not-utf8", "L5W112Da", () -> {
            K k = new K("L5W112Da");
            int sde = k.utf8("SourceDebugExtension");
            addM(k);
            k.attrs.add(attr(sde, b(0xFF, 0xFE, 0x00)));
            return k.build();
        });
    }
}
