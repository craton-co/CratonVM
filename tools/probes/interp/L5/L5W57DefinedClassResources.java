// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 57, lane L5 -- a class DEFINED at run time has no
// `.class` resource on HotSpot: no loader's resource lookup knows bytes that
// came from `defineClass`
// (`docs/internal/fixed-bugs/interpreter-L5-proposal-defined-class-resource-bytes-ask-the-defining-loader-FIXED-20261021.md`).
// The probe builds a minimal class file in memory (no class-path copy), then
// defines it twice, under two names:
//
//   lookup-*  `MethodHandles.lookup().defineClass(bytes)` (the application
//             loader, the probe's own package)
//   user-*    a user loader's `defineClass` (parent: the application loader)
//
// Each row asks four doors for "<name>.class", printing `null` or the stream's
// first four bytes:
//   sys     `ClassLoader.getSystemResourceAsStream`
//   class   `<defined>.class.getResourceAsStream("<name>.class")`
//   module  `<defined>.class.getModule().getResourceAsStream("<name>.class")`
//   loader  `<defined>.class.getClassLoader().getResourceAsStream(...)`
//
// Run (no setup):
//   javac -d out L5W57DefinedClassResources.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W57DefinedClassResources
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   lookup-1=sys=null class=null module=null loader=null
//   user-1=sys=null class=null module=null loader=null
//   lookup-2=sys=null class=null module=null loader=null
//   user-2=sys=null class=null module=null loader=null
//
// CratonVM with `CRATONVM_DEFINED_CLASS_RESOURCES_DEFINING_LOADER=0` (debug
// build, Windows, measured): `--jdk-only` `sys=cafebabe` on both rows (the
// static door served any loaded class's bytes); `--compatible` `sys=cafebabe`
// and `module=cafebabe` on both rows. With the switch, HotSpot's lines.

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.InputStream;
import java.lang.invoke.MethodHandles;
import java.util.concurrent.Callable;

public class L5W57DefinedClassResources {
    static byte[] classFile(String name) throws Exception {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(52);
        out.writeShort(5); // constant pool count
        out.writeByte(7);
        out.writeShort(2); // #1 Class #2
        out.writeByte(1);
        out.writeUTF(name); // #2
        out.writeByte(7);
        out.writeShort(4); // #3 Class #4
        out.writeByte(1);
        out.writeUTF("java/lang/Object"); // #4
        out.writeShort(0x21); // public super
        out.writeShort(1);
        out.writeShort(3);
        out.writeShort(0); // interfaces
        out.writeShort(0); // fields
        out.writeShort(0); // methods
        out.writeShort(0); // attributes
        out.flush();
        return bytes.toByteArray();
    }

    static final class Definer extends ClassLoader {
        Definer(ClassLoader parent) {
            super(parent);
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static String attempt(Callable<Object> c) {
        try {
            return String.valueOf(c.call());
        } catch (Throwable t) {
            return t.getClass().getName() + " msg=" + t.getMessage();
        }
    }

    static String head(InputStream in) throws Exception {
        if (in == null) {
            return "null";
        }
        try (in) {
            byte[] b = in.readNBytes(4);
            return String.format("%02x%02x%02x%02x", b[0], b[1], b[2], b[3]);
        }
    }

    static String doors(Class<?> c) {
        String res = c.getName() + ".class";
        return "sys=" + attempt(() -> head(ClassLoader.getSystemResourceAsStream(res)))
                + " class=" + attempt(() -> head(c.getResourceAsStream(res)))
                + " module=" + attempt(() -> head(c.getModule().getResourceAsStream(res)))
                + " loader=" + attempt(() -> head(c.getClassLoader().getResourceAsStream(res)));
    }

    public static void main(String[] args) throws Exception {
        Class<?> a = MethodHandles.lookup().defineClass(classFile("L5W57GenLookup"));
        Class<?> b = new Definer(ClassLoader.getSystemClassLoader())
                .define("L5W57GenUser", classFile("L5W57GenUser"));
        for (String tag : new String[] {"1", "2"}) {
            System.out.println("lookup-" + tag + "=" + doors(a));
            System.out.println("user-" + tag + "=" + doors(b));
        }
    }
}
