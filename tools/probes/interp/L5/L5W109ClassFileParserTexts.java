// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, wave 109, lane L5 -- HotSpot 25's `ClassFileParser`
// ClassFormatError texts for class files a user-defined loader hands to
// `defineClass` (`_need_verify` on, not relaxed). Pages:
// interpreter-i108-L7-code-attribute-parse-refusals-use-the-readers-words-FIXED-20261016 (rows
// code-length-0, handler-range, handler-pc) and the wave-109 audit of the
// reader's other refusals. Every class is hand-assembled from raw bytes
// (version 69, `public class X extends Object` with one static method `m()V`
// unless the row says otherwise), each with exactly one defect, and defined by
// a fresh loader under its binary name; the row prints `Throwable.toString()`.
// CratonVM printed `X: invalid class data: <reader words>` (or `X: invalid
// constant pool entry: ...`, `X: unexpected end of data at position N`, ...)
// for the reader rows, and DEFINED the classes of the define-time rows
// (illegal-*, *-signature, too-many-args, lnt-*, lvt-*, nest-*, permitted-*,
// dup-interface).
//
// Run (no setup):
//   javac -d out L5W109ClassFileParserTexts.java
//   java -cp out L5W109ClassFileParserTexts                             (HotSpot 25)
//   cratonvm --jdk-only   [--nojit] -cp out L5W109ClassFileParserTexts
//   cratonvm --compatible [--nojit] -cp out L5W109ClassFileParserTexts
//
// HOTSPOT_EXPECTED predicted from the JDK 25 sources (classFileParser.cpp,
// classFileStream.cpp), not run.
// Wave 109 orchestrator run (Windows): 46 of 49 rows SAME; the two name rows
// were corrected to HotSpot's measured `in class <C>` (no `file`).
// HOTSPOT_EXPECTED_BEGIN
// ok -> defined L5W109Ok
// code-length-0 -> java.lang.ClassFormatError: Invalid method Code length 0 in class file L5W109Cz
// handler-range -> java.lang.ClassFormatError: Illegal exception table range in class file L5W109Er
// handler-empty-range -> java.lang.ClassFormatError: Illegal exception table range in class file L5W109Ee
// handler-pc -> java.lang.ClassFormatError: Illegal exception table handler in class file L5W109Eh
// catch-type -> java.lang.ClassFormatError: Catch type in exception table has bad constant type in class file L5W109Ct
// code-segment -> java.lang.ClassFormatError: Code segment has wrong length in class file L5W109Cs
// cp-size-0 -> java.lang.ClassFormatError: Illegal constant pool size 0 in class file L5W109Cp
// unknown-tag -> java.lang.ClassFormatError: Unknown constant tag 2 in class file L5W109Ut
// illegal-utf8 -> java.lang.ClassFormatError: Illegal UTF8 string in constant pool in class file L5W109U8
// long-last-slot -> java.lang.ClassFormatError: Invalid constant pool entry 8 in class file L5W109Ll
// class-name-index -> java.lang.ClassFormatError: Invalid constant pool index 8 in class file L5W109Ci
// string-index -> java.lang.ClassFormatError: Invalid constant pool index 2 in class file L5W109Si
// this-class -> java.lang.ClassFormatError: Invalid this class index 1 in constant pool in class file L5W109Tc
// super-class -> java.lang.ClassFormatError: Invalid superclass index 1 in class file L5W109Sc
// interface -> java.lang.ClassFormatError: Interface name has bad constant pool index 1 in class file L5W109If
// dup-interface -> java.lang.ClassFormatError: Duplicate interface name "java/lang/Runnable" in class file L5W109Di
// field-name-index -> java.lang.ClassFormatError: Invalid constant pool index 2 for field name in class file L5W109Fn
// field-sig-index -> java.lang.ClassFormatError: Invalid constant pool index 2 for field signature in class file L5W109Fd
// method-name-index -> java.lang.ClassFormatError: Illegal constant pool index 2 for method name in class file L5W109Mn
// method-sig-index -> java.lang.ClassFormatError: Illegal constant pool index 2 for method signature in class file L5W109Md
// field-attr-index -> java.lang.ClassFormatError: Invalid field attribute index 2 in class file L5W109Fa
// method-attr-index -> java.lang.ClassFormatError: Invalid method attribute name index 2 in class file L5W109Ma
// class-attr-index -> java.lang.ClassFormatError: Attribute name has bad constant pool index 2 in class file L5W109Ka
// truncated -> java.lang.ClassFormatError: Truncated class file
// seven-bytes -> java.lang.ClassFormatError: Truncated class file
// illegal-field-name -> java.lang.ClassFormatError: Illegal field name "a.b" in class L5W109Fi
// illegal-method-name -> java.lang.ClassFormatError: Illegal method name "a/b" in class L5W109Mi
// field-signature -> java.lang.ClassFormatError: Field "f" in class L5W109Fs has illegal signature "Q"
// method-signature -> java.lang.ClassFormatError: Method "m" in class L5W109Ms has illegal signature "(I"
// init-signature -> java.lang.ClassFormatError: Method "<init>" in class L5W109Is has illegal signature "()I"
// too-many-args -> java.lang.ClassFormatError: Too many arguments in method signature in class file L5W109Ta
// lnt-pc -> java.lang.ClassFormatError: Invalid pc in LineNumberTable in class file L5W109Ln
// lvt-start -> java.lang.ClassFormatError: Invalid start_pc 7 in LocalVariableTable in class file L5W109Vs
// lvt-length -> java.lang.ClassFormatError: Invalid length 9 in LocalVariableTable in class file L5W109Vl
// lvt-index -> java.lang.ClassFormatError: Invalid index 5 in LocalVariableTable in class file L5W109Vi
// lvt-name -> java.lang.ClassFormatError: Illegal field name "a.b" in class L5W109Vn
// constant-value-length -> java.lang.ClassFormatError: Invalid ConstantValue field attribute length 3 in class file L5W109Cv
// enclosing-length -> java.lang.ClassFormatError: Wrong EnclosingMethod attribute length 5 in class file L5W109Em
// nest-host-length -> java.lang.ClassFormatError: Wrong NestHost attribute length in class file L5W109Nl
// nest-host-type -> java.lang.ClassFormatError: Nest-host class_info_index 1 has bad constant type in class file L5W109Nh
// nest-conflict -> java.lang.ClassFormatError: Conflicting NestMembers and NestHost attributes in class file L5W109Nc
// nest-members-twice -> java.lang.ClassFormatError: Multiple NestMembers attributes in class file L5W109Nm
// nest-members-length -> java.lang.ClassFormatError: Wrong NestMembers attribute length in class file L5W109Nw
// nest-member-type -> java.lang.ClassFormatError: Nest member class_info_index 1 has bad constant type in class file L5W109Nt
// permitted-final -> java.lang.ClassFormatError: PermittedSubclasses attribute in final class file L5W109Pf
// permitted-type -> java.lang.ClassFormatError: Permitted subclass class_info_index 1 has bad constant type in class file L5W109Pt
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

public class L5W109ClassFileParserTexts {

    /** Raw class-file writer. Constant-pool entries are appended in call order. */
    static final class K {
        final ByteArrayOutputStream cpBytes = new ByteArrayOutputStream();
        final DataOutputStream cp = new DataOutputStream(cpBytes);
        int next = 1;
        int flags = 0x21;
        int thisIdx;
        int superIdx;
        final List<Integer> ifaces = new ArrayList<>();
        final List<byte[]> fields = new ArrayList<>();
        final List<byte[]> methods = new ArrayList<>();
        final List<byte[]> attrs = new ArrayList<>();
        Integer cpCount;

        /** Index 1 is always the Utf8 of `name`, index 2 its Class, 3 / 4 Object's. */
        K(String name) throws IOException {
            int u = utf8(name);
            thisIdx = cls(u);
            int o = utf8("java/lang/Object");
            superIdx = cls(o);
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

        int cls(String name) throws IOException {
            return cls(utf8(name));
        }

        int integer(int v) throws IOException {
            cp.writeByte(3);
            cp.writeInt(v);
            return next++;
        }

        int string(int utf8Idx) throws IOException {
            cp.writeByte(8);
            cp.writeShort(utf8Idx);
            return next++;
        }

        /** A Long that takes one slot only (the last-slot defect). */
        void longOneSlot() throws IOException {
            cp.writeByte(5);
            cp.writeLong(1L);
            next++;
        }

        /** A CONSTANT_Utf8 of raw (possibly malformed) bytes. */
        void rawUtf8(byte[] bytes) throws IOException {
            cp.writeByte(1);
            cp.writeShort(bytes.length);
            cp.write(bytes);
            next++;
        }

        void rawTag(int tag) throws IOException {
            cp.writeByte(tag);
            cp.writeShort(0);
            next++;
        }

        byte[] build() throws IOException {
            ByteArrayOutputStream bytes = new ByteArrayOutputStream();
            DataOutputStream out = new DataOutputStream(bytes);
            out.writeInt(0xCAFEBABE);
            out.writeShort(0);
            out.writeShort(69);
            out.writeShort(cpCount != null ? cpCount : next);
            cp.flush();
            out.write(cpBytes.toByteArray());
            out.writeShort(flags);
            out.writeShort(thisIdx);
            out.writeShort(superIdx);
            out.writeShort(ifaces.size());
            for (int i : ifaces) {
                out.writeShort(i);
            }
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

    static byte[] cat(byte[]... parts) {
        ByteArrayOutputStream b = new ByteArrayOutputStream();
        for (byte[] p : parts) {
            b.write(p, 0, p.length);
        }
        return b.toByteArray();
    }

    static byte[] b(int... v) {
        byte[] r = new byte[v.length];
        for (int i = 0; i < v.length; i++) {
            r[i] = (byte) v[i];
        }
        return r;
    }

    /** attribute_info with a declared length that may differ from the body. */
    static byte[] attr(int nameIdx, int declaredLen, byte[] body) throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeShort(nameIdx);
        out.writeInt(declaredLen);
        out.write(body);
        return bytes.toByteArray();
    }

    static byte[] attr(int nameIdx, byte[] body) throws IOException {
        return attr(nameIdx, body.length, body);
    }

    /** field_info / method_info. */
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

    /** A Code body; `pad` trailing bytes follow the structured fields. */
    static byte[] codeBody(int maxStack, int maxLocals, byte[] code, int[][] handlers, int pad,
            byte[]... nested) throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
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
        out.writeShort(nested.length);
        for (byte[] n : nested) {
            out.write(n);
        }
        out.write(new byte[pad]);
        return bytes.toByteArray();
    }

    static final int[][] NO_HANDLERS = new int[0][];

    /** `public static m()V` with `code` and `handlers`. */
    static void addM(K k, int maxStack, int maxLocals, byte[] code, int[][] handlers, int pad,
            byte[]... nested) throws IOException {
        int code_u = k.utf8("Code");
        k.methods.add(member(0x0009, k.utf8("m"), k.utf8("()V"),
            attr(code_u, codeBody(maxStack, maxLocals, code, handlers, pad, nested))));
    }

    static void addM(K k) throws IOException {
        addM(k, 0, 0, b(0xb1), NO_HANDLERS, 0);
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L5W109ClassFileParserTexts.class.getClassLoader());
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
            Class<?> c = new Loader().def(name, bytes);
            out = "defined " + c.getName();
        } catch (Throwable t) {
            out = String.valueOf(t).split("\n")[0];
        }
        System.out.println(label + " -> " + out);
    }

    public static void main(String[] args) throws Exception {
        row("ok", "L5W109Ok", () -> {
            K k = new K("L5W109Ok");
            addM(k);
            return k.build();
        });

        // --- Code attribute (parse_method / parse_exception_table) ---
        row("code-length-0", "L5W109Cz", () -> {
            K k = new K("L5W109Cz");
            addM(k, 0, 0, new byte[0], NO_HANDLERS, 0);
            return k.build();
        });
        // 0 aconst_null; 1 athrow
        row("handler-range", "L5W109Er", () -> {
            K k = new K("L5W109Er");
            addM(k, 1, 0, b(0x01, 0xbf), new int[][] {{0, 9, 0, 0}}, 0);
            return k.build();
        });
        row("handler-empty-range", "L5W109Ee", () -> {
            K k = new K("L5W109Ee");
            addM(k, 1, 0, b(0x01, 0xbf), new int[][] {{1, 1, 0, 0}}, 0);
            return k.build();
        });
        row("handler-pc", "L5W109Eh", () -> {
            K k = new K("L5W109Eh");
            addM(k, 1, 0, b(0x01, 0xbf), new int[][] {{0, 1, 9, 0}}, 0);
            return k.build();
        });
        row("catch-type", "L5W109Ct", () -> {
            K k = new K("L5W109Ct");
            // catch_type 1 is the class's name Utf8, not a CONSTANT_Class.
            addM(k, 1, 0, b(0x01, 0xbf), new int[][] {{0, 1, 0, 1}}, 0);
            return k.build();
        });
        row("code-segment", "L5W109Cs", () -> {
            K k = new K("L5W109Cs");
            addM(k, 0, 0, b(0xb1), NO_HANDLERS, 2);
            return k.build();
        });

        // --- constant pool (parse_stream / parse_constant_pool[_entries]) ---
        row("cp-size-0", "L5W109Cp", () -> {
            K k = new K("L5W109Cp");
            addM(k);
            k.cpCount = 0;
            return k.build();
        });
        row("unknown-tag", "L5W109Ut", () -> {
            K k = new K("L5W109Ut");
            addM(k);
            k.rawTag(2);
            return k.build();
        });
        row("illegal-utf8", "L5W109U8", () -> {
            K k = new K("L5W109U8");
            addM(k);
            k.rawUtf8(b(0x41, 0xF0, 0x90, 0x80, 0x80)); // a 4-byte sequence
            return k.build();
        });
        row("long-last-slot", "L5W109Ll", () -> {
            K k = new K("L5W109Ll");
            addM(k); // 5 Code, 6 m, 7 ()V
            k.longOneSlot(); // 8, the last slot
            return k.build();
        });
        row("class-name-index", "L5W109Ci", () -> {
            K k = new K("L5W109Ci");
            addM(k); // 5 Code, 6 m, 7 ()V
            int i = k.integer(42); // 8
            k.cls(i); // 9: a Class naming an Integer
            return k.build();
        });
        row("string-index", "L5W109Si", () -> {
            K k = new K("L5W109Si");
            addM(k);
            k.string(2); // a String naming the CONSTANT_Class at 2
            return k.build();
        });

        // --- this / super / interfaces (parse_stream, parse_super_class, parse_interfaces) ---
        row("this-class", "L5W109Tc", () -> {
            K k = new K("L5W109Tc");
            addM(k);
            k.thisIdx = 1;
            return k.build();
        });
        row("super-class", "L5W109Sc", () -> {
            K k = new K("L5W109Sc");
            addM(k);
            k.superIdx = 1;
            return k.build();
        });
        row("interface", "L5W109If", () -> {
            K k = new K("L5W109If");
            addM(k);
            k.ifaces.add(1);
            return k.build();
        });
        row("dup-interface", "L5W109Di", () -> {
            K k = new K("L5W109Di");
            k.flags = 0x0421; // abstract: no run()
            int r = k.cls("java/lang/Runnable");
            k.ifaces.add(r);
            k.ifaces.add(r);
            addM(k);
            return k.build();
        });

        // --- member and attribute name indexes ---
        row("field-name-index", "L5W109Fn", () -> {
            K k = new K("L5W109Fn");
            k.fields.add(member(0x0009, 2, k.utf8("I")));
            addM(k);
            return k.build();
        });
        row("field-sig-index", "L5W109Fd", () -> {
            K k = new K("L5W109Fd");
            k.fields.add(member(0x0009, k.utf8("f"), 2));
            addM(k);
            return k.build();
        });
        row("method-name-index", "L5W109Mn", () -> {
            K k = new K("L5W109Mn");
            k.methods.add(member(0x0109, 2, k.utf8("()V")));
            return k.build();
        });
        row("method-sig-index", "L5W109Md", () -> {
            K k = new K("L5W109Md");
            k.methods.add(member(0x0109, k.utf8("m"), 2));
            return k.build();
        });
        row("field-attr-index", "L5W109Fa", () -> {
            K k = new K("L5W109Fa");
            k.fields.add(member(0x0009, k.utf8("f"), k.utf8("I"), attr(2, new byte[0])));
            addM(k);
            return k.build();
        });
        row("method-attr-index", "L5W109Ma", () -> {
            K k = new K("L5W109Ma");
            k.methods.add(member(0x0109, k.utf8("m"), k.utf8("()V"), attr(2, new byte[0])));
            return k.build();
        });
        row("class-attr-index", "L5W109Ka", () -> {
            K k = new K("L5W109Ka");
            addM(k);
            k.attrs.add(attr(2, new byte[0]));
            return k.build();
        });

        // --- truncation (ClassFileStream::truncated_file_error) ---
        row("truncated", "L5W109Tr", () -> {
            K k = new K("L5W109Tr");
            addM(k);
            byte[] whole = k.build();
            return Arrays.copyOf(whole, whole.length - 1);
        });
        row("seven-bytes", "L5W109Sv", () -> b(0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0));

        // --- member names and descriptors (verify_legal_*) ---
        row("illegal-field-name", "L5W109Fi", () -> {
            K k = new K("L5W109Fi");
            k.fields.add(member(0x0009, k.utf8("a.b"), k.utf8("I")));
            addM(k);
            return k.build();
        });
        row("illegal-method-name", "L5W109Mi", () -> {
            K k = new K("L5W109Mi");
            k.methods.add(member(0x0109, k.utf8("a/b"), k.utf8("()V")));
            return k.build();
        });
        row("field-signature", "L5W109Fs", () -> {
            K k = new K("L5W109Fs");
            k.fields.add(member(0x0009, k.utf8("f"), k.utf8("Q")));
            addM(k);
            return k.build();
        });
        row("method-signature", "L5W109Ms", () -> {
            K k = new K("L5W109Ms");
            k.methods.add(member(0x0109, k.utf8("m"), k.utf8("(I")));
            return k.build();
        });
        row("init-signature", "L5W109Is", () -> {
            K k = new K("L5W109Is");
            int code_u = k.utf8("Code");
            k.methods.add(member(0x0001, k.utf8("<init>"), k.utf8("()I"),
                attr(code_u, codeBody(1, 1, b(0x03, 0xac), NO_HANDLERS, 0))));
            return k.build();
        });
        row("too-many-args", "L5W109Ta", () -> {
            K k = new K("L5W109Ta");
            StringBuilder sig = new StringBuilder("(");
            for (int i = 0; i < 256; i++) {
                sig.append('I');
            }
            sig.append(")V");
            k.methods.add(member(0x0109, k.utf8("m"), k.utf8(sig.toString())));
            return k.build();
        });

        // --- Code's LineNumberTable / LocalVariableTable (_need_verify) ---
        row("lnt-pc", "L5W109Ln", () -> {
            K k = new K("L5W109Ln");
            int lnt = k.utf8("LineNumberTable");
            addM(k, 0, 0, b(0xb1), NO_HANDLERS, 0, attr(lnt, u2(1, 5, 1)));
            return k.build();
        });
        row("lvt-start", "L5W109Vs", () -> {
            K k = new K("L5W109Vs");
            int lvt = k.utf8("LocalVariableTable");
            int x = k.utf8("x");
            int i = k.utf8("I");
            addM(k, 0, 1, b(0xb1), NO_HANDLERS, 0, attr(lvt, u2(1, 7, 0, x, i, 0)));
            return k.build();
        });
        row("lvt-length", "L5W109Vl", () -> {
            K k = new K("L5W109Vl");
            int lvt = k.utf8("LocalVariableTable");
            int x = k.utf8("x");
            int i = k.utf8("I");
            addM(k, 0, 1, b(0xb1), NO_HANDLERS, 0, attr(lvt, u2(1, 0, 9, x, i, 0)));
            return k.build();
        });
        row("lvt-index", "L5W109Vi", () -> {
            K k = new K("L5W109Vi");
            int lvt = k.utf8("LocalVariableTable");
            int x = k.utf8("x");
            int i = k.utf8("I");
            addM(k, 0, 1, b(0xb1), NO_HANDLERS, 0, attr(lvt, u2(1, 0, 1, x, i, 5)));
            return k.build();
        });
        row("lvt-name", "L5W109Vn", () -> {
            K k = new K("L5W109Vn");
            int lvt = k.utf8("LocalVariableTable");
            int x = k.utf8("a.b");
            int i = k.utf8("I");
            addM(k, 0, 1, b(0xb1), NO_HANDLERS, 0, attr(lvt, u2(1, 0, 1, x, i, 0)));
            return k.build();
        });

        // --- fixed-length attributes ---
        row("constant-value-length", "L5W109Cv", () -> {
            K k = new K("L5W109Cv");
            int cv = k.utf8("ConstantValue");
            int v = k.integer(1);
            k.fields.add(member(0x0019, k.utf8("f"), k.utf8("I"), attr(cv, cat(u2(v), b(0)))));
            addM(k);
            return k.build();
        });
        row("enclosing-length", "L5W109Em", () -> {
            K k = new K("L5W109Em");
            int em = k.utf8("EnclosingMethod");
            addM(k);
            k.attrs.add(attr(em, cat(u2(4, 0), b(0))));
            return k.build();
        });

        // --- nest and sealed attributes (parse_classfile_attributes) ---
        row("nest-host-length", "L5W109Nl", () -> {
            K k = new K("L5W109Nl");
            int nh = k.utf8("NestHost");
            addM(k);
            k.attrs.add(attr(nh, cat(u2(4), b(0))));
            return k.build();
        });
        row("nest-host-type", "L5W109Nh", () -> {
            K k = new K("L5W109Nh");
            int nh = k.utf8("NestHost");
            addM(k);
            k.attrs.add(attr(nh, u2(1)));
            return k.build();
        });
        row("nest-conflict", "L5W109Nc", () -> {
            K k = new K("L5W109Nc");
            int nm = k.utf8("NestMembers");
            int nh = k.utf8("NestHost");
            addM(k);
            k.attrs.add(attr(nm, u2(1, 2)));
            k.attrs.add(attr(nh, u2(4)));
            return k.build();
        });
        row("nest-members-twice", "L5W109Nm", () -> {
            K k = new K("L5W109Nm");
            int nm = k.utf8("NestMembers");
            addM(k);
            k.attrs.add(attr(nm, u2(1, 2)));
            k.attrs.add(attr(nm, u2(1, 2)));
            return k.build();
        });
        row("nest-members-length", "L5W109Nw", () -> {
            K k = new K("L5W109Nw");
            int nm = k.utf8("NestMembers");
            addM(k);
            k.attrs.add(attr(nm, u2(1, 2, 0)));
            return k.build();
        });
        row("nest-member-type", "L5W109Nt", () -> {
            K k = new K("L5W109Nt");
            int nm = k.utf8("NestMembers");
            addM(k);
            k.attrs.add(attr(nm, u2(1, 1)));
            return k.build();
        });
        row("permitted-final", "L5W109Pf", () -> {
            K k = new K("L5W109Pf");
            k.flags = 0x0031;
            int ps = k.utf8("PermittedSubclasses");
            addM(k);
            k.attrs.add(attr(ps, u2(1, 4)));
            return k.build();
        });
        row("permitted-type", "L5W109Pt", () -> {
            K k = new K("L5W109Pt");
            int ps = k.utf8("PermittedSubclasses");
            addM(k);
            k.attrs.add(attr(ps, u2(1, 1)));
            return k.build();
        });
    }
}
