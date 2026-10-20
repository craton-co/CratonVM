// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4, wave 84, lane L5 -- HotSpot `ClassFileParser`'s
// access-flag refusals for a class a user loader defines
// (`classloading/src/class_manager.rs` `class_modifier_refusal` /
// `member_modifier_refusal`).
//
// Each case hand-writes a version-69 class file and hands it to
// `ClassLoader.defineClass`. HotSpot refuses the flags while it parses
// (`verify_legal_class_modifiers`, `verify_legal_field_modifiers`,
// `verify_legal_method_modifiers`, the `<clinit>` / interface `<init>` checks
// in `parse_method`, the ACC_MODULE check), in that order: class flags right
// after the constant pool (so before the wrong-name check, naming the
// REQUESTED name), then every field, then every method. CratonVM accepted
// most of these shapes before wave 84 (a user loader's class skips the
// link-time structural pass), defined `module-info` as a class, and worded
// the rest as a VerifyError or `<name>: class cannot be both FINAL and
// ABSTRACT`.
//
// Run:
//   $J/javac -d out L5W84ClassFileModifiers.java
//   $J/java -cp out L5W84ClassFileModifiers                 # HotSpot
//   $B --java-home $J/.. -cp out L5W84ClassFileModifiers
//   $B --compatible --java-home $J/.. -cp out L5W84ClassFileModifiers
// The expected block is derived from JDK 25's classFileParser.cpp; confirm it
// on the host first.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags, both CratonVM modes)
// final-abstract: java.lang.ClassFormatError: Illegal class modifiers in class M1: 0x431
// interface-no-abstract: java.lang.ClassFormatError: Illegal class modifiers in class M2: 0x201
// interface-super: java.lang.ClassFormatError: Illegal class modifiers in class M3: 0x621
// annotation-class: java.lang.ClassFormatError: Illegal class modifiers in class M4: 0x2021
// module-info: java.lang.NoClassDefFoundError: module-info is not a class because access_flag ACC_MODULE is set
// field-two-visibilities: java.lang.ClassFormatError: Illegal field modifiers in class M6: 0x3
// field-final-volatile: java.lang.ClassFormatError: Illegal field modifiers in class M7: 0x50
// interface-field-not-static: java.lang.ClassFormatError: Illegal field modifiers in class M8: 0x11
// abstract-private-method: java.lang.ClassFormatError: Method m in class M9 has illegal modifiers: 0x402
// interface-protected-method: java.lang.ClassFormatError: Method m in class M10 has illegal modifiers: 0x404
// interface-package-method: java.lang.ClassFormatError: Method m in class M11 has illegal modifiers: 0x400
// static-init: java.lang.ClassFormatError: Method <init> in class M12 has illegal modifiers: 0x9
// clinit-not-static: java.lang.ClassFormatError: Method <clinit> is not static in class file M13
// interface-init: java.lang.ClassFormatError: Interface cannot have a method named <init>, class file M14
// field-before-method: java.lang.ClassFormatError: Illegal field modifiers in class M15: 0x6
// flags-before-wrong-name: java.lang.ClassFormatError: Illegal class modifiers in class M16: 0x431
// legal-interface: defined M17 interface=true
// legal-abstract-class: defined M18 interface=false
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;

public class L5W84ClassFileModifiers {
    static final class Loader extends ClassLoader {
        Loader() {
            super(L5W84ClassFileModifiers.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    /** One member: flags, name, descriptor. */
    record Member(int flags, String name, String desc) {}

    /**
     * A version-69 class file: `thisName` with `classFlags`, superclass
     * java/lang/Object (none for a module-info), the given fields and
     * methods, no attributes anywhere.
     */
    static byte[] classFile(String thisName, int classFlags, boolean noSuper,
                            Member[] fields, Member[] methods) throws IOException {
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bos);
        // Constant pool: 1 Utf8 this, 2 Class this, 3 Utf8 Object, 4 Class Object,
        // then a Utf8 name and a Utf8 descriptor per member.
        int members = fields.length + methods.length;
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(69);
        out.writeShort(5 + 2 * members);
        out.writeByte(1);
        out.writeUTF(thisName);
        out.writeByte(7);
        out.writeShort(1);
        out.writeByte(1);
        out.writeUTF("java/lang/Object");
        out.writeByte(7);
        out.writeShort(3);
        Member[] all = new Member[members];
        System.arraycopy(fields, 0, all, 0, fields.length);
        System.arraycopy(methods, 0, all, fields.length, methods.length);
        for (Member m : all) {
            out.writeByte(1);
            out.writeUTF(m.name());
            out.writeByte(1);
            out.writeUTF(m.desc());
        }
        out.writeShort(classFlags);
        out.writeShort(2);
        out.writeShort(noSuper ? 0 : 4);
        out.writeShort(0); // interfaces
        int cp = 5;
        out.writeShort(fields.length);
        for (Member f : fields) {
            out.writeShort(f.flags());
            out.writeShort(cp++);
            out.writeShort(cp++);
            out.writeShort(0);
        }
        out.writeShort(methods.length);
        for (Member m : methods) {
            out.writeShort(m.flags());
            out.writeShort(cp++);
            out.writeShort(cp++);
            out.writeShort(0);
        }
        out.writeShort(0); // class attributes
        out.flush();
        return bos.toByteArray();
    }

    static final Member[] NONE = new Member[0];

    static Member[] of(Member... m) {
        return m;
    }

    static void run(String label, String requested, String thisName, int flags, boolean noSuper,
                    Member[] fields, Member[] methods) {
        try {
            byte[] b = classFile(thisName, flags, noSuper, fields, methods);
            Class<?> c = new Loader().define(requested, b);
            System.out.println(label + ": defined " + c.getName() + " interface=" + c.isInterface());
        } catch (Throwable t) {
            System.out.println(label + ": " + t.getClass().getName() + ": " + t.getMessage());
        }
    }

    static void run(String label, String name, int flags, Member[] fields, Member[] methods) {
        run(label, name, name, flags, false, fields, methods);
    }

    public static void main(String[] args) {
        run("final-abstract", "M1", 0x0431, NONE, NONE);
        run("interface-no-abstract", "M2", 0x0201, NONE, NONE);
        run("interface-super", "M3", 0x0621, NONE, NONE);
        run("annotation-class", "M4", 0x2021, NONE, NONE);
        run("module-info", "module-info", "module-info", 0x8000, true, NONE, NONE);
        run("field-two-visibilities", "M6", 0x0021, of(new Member(0x0003, "f", "I")), NONE);
        run("field-final-volatile", "M7", 0x0021, of(new Member(0x0050, "f", "I")), NONE);
        run("interface-field-not-static", "M8", 0x0601, of(new Member(0x0011, "f", "I")), NONE);
        run("abstract-private-method", "M9", 0x0421, NONE, of(new Member(0x0402, "m", "()V")));
        run("interface-protected-method", "M10", 0x0601, NONE, of(new Member(0x0404, "m", "()V")));
        run("interface-package-method", "M11", 0x0601, NONE, of(new Member(0x0400, "m", "()V")));
        run("static-init", "M12", 0x0021, NONE, of(new Member(0x0009, "<init>", "()V")));
        run("clinit-not-static", "M13", 0x0021, NONE, of(new Member(0x0000, "<clinit>", "()V")));
        run("interface-init", "M14", 0x0601, NONE, of(new Member(0x0001, "<init>", "()V")));
        run("field-before-method", "M15", 0x0421,
            of(new Member(0x0006, "f", "I")), of(new Member(0x0402, "m", "()V")));
        run("flags-before-wrong-name", "M16", "Other16", 0x0431, false, NONE, NONE);
        run("legal-interface", "M17", 0x0601,
            of(new Member(0x0019, "F", "I")), of(new Member(0x0401, "m", "()V")));
        run("legal-abstract-class", "M18", 0x0421,
            of(new Member(0x0002, "f", "I")), of(new Member(0x0401, "m", "()V")));
    }
}
