// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, wave 106, lane L5 -- HotSpot's class-name checks in
// `ClassFileParser` for a user loader (`_need_verify`, not relaxed):
// `verify_legal_class_name` on every CONSTANT_Class while the constant pool is
// parsed (`Illegal class name "<n>" in class file <requested>`; an array
// descriptor whose element is malformed: `Class name is empty or contains
// illegal character in descriptor in class file <requested>`), then
// `this_class` must not be an array (`Bad class name in class file
// <requested>`). `<requested>` is the internal requested name, `<Unknown>`
// for a null one. CratonVM checked none of these and DEFINED the class (a
// class named `[Ll5w106/A;`, `l5w106.Dotted`, ...). Fix:
// `classloading/src/class_manager.rs` `class_name_refusal`. Each class file is
// `public class <this_class> extends java/lang/Object {}`, version 52.
//
// Run (no setup):
//   javac -d out L5W106IllegalClassNames.java
//   java -cp out L5W106IllegalClassNames                           (HotSpot 25)
//   cratonvm --jdk-only   [--nojit] -cp out L5W106IllegalClassNames
//   cratonvm --compatible [--nojit] -cp out L5W106IllegalClassNames
//
// HOTSPOT_EXPECTED derived from the JDK 25 sources (classFileParser.cpp), not run.
// HOTSPOT_EXPECTED_BEGIN
// dotted=java.lang.ClassFormatError: Illegal class name "l5w106.Dotted" in class file <Unknown>
// double-slash=java.lang.ClassFormatError: Illegal class name "l5w106//B" in class file <Unknown>
// leading-slash=java.lang.ClassFormatError: Illegal class name "/l5w106/B" in class file <Unknown>
// trailing-slash=java.lang.ClassFormatError: Illegal class name "l5w106/" in class file <Unknown>
// semicolon=java.lang.ClassFormatError: Illegal class name "l5w106/A;B" in class file <Unknown>
// empty=java.lang.ClassFormatError: Illegal class name "" in class file <Unknown>
// array=java.lang.ClassFormatError: Bad class name in class file <Unknown>
// array-bad-element=java.lang.ClassFormatError: Class name is empty or contains illegal character in descriptor in class file <Unknown>
// named-double-slash=java.lang.ClassFormatError: Illegal class name "l5w106//B" in class file l5w106/Ok
// legal=defined l5w106.Fine
// HOTSPOT_EXPECTED_END

public class L5W106IllegalClassNames {
    static final class Open extends ClassLoader {
        Open() {
            super(null);
        }

        Class<?> def(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    /** `public class <name> extends java/lang/Object {}`, version 52. */
    static byte[] classBytes(String name) {
        java.io.ByteArrayOutputStream bytes = new java.io.ByteArrayOutputStream();
        java.io.DataOutputStream out = new java.io.DataOutputStream(bytes);
        try {
            out.writeInt(0xCAFEBABE);
            out.writeShort(0);
            out.writeShort(52);
            out.writeShort(5);
            out.writeByte(1);
            out.writeUTF(name);
            out.writeByte(7);
            out.writeShort(1);
            out.writeByte(1);
            out.writeUTF("java/lang/Object");
            out.writeByte(7);
            out.writeShort(3);
            out.writeShort(0x21);
            out.writeShort(2);
            out.writeShort(4);
            out.writeShort(0);
            out.writeShort(0);
            out.writeShort(0);
            out.writeShort(0);
        } catch (java.io.IOException e) {
            throw new RuntimeException(e);
        }
        return bytes.toByteArray();
    }

    static void row(String label, String requested, String thisClass) {
        String v;
        try {
            v = "defined " + new Open().def(requested, classBytes(thisClass)).getName();
        } catch (Throwable t) {
            v = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(label + "=" + v);
    }

    public static void main(String[] args) {
        row("dotted", null, "l5w106.Dotted");
        row("double-slash", null, "l5w106//B");
        row("leading-slash", null, "/l5w106/B");
        row("trailing-slash", null, "l5w106/");
        row("semicolon", null, "l5w106/A;B");
        row("empty", null, "");
        row("array", null, "[Ll5w106/A;");
        row("array-bad-element", null, "[Ll5w106.A;");
        row("named-double-slash", "l5w106.Ok", "l5w106//B");
        row("legal", null, "l5w106/Fine");
    }
}
