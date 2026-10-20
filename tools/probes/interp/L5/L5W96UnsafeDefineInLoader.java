// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4, wave 96, lane L5 -- `Unsafe.defineClass` from a user
// loader defines the class IN that loader
// (`docs/internal/fixed-bugs/interpreter-i95-L5-define-door-loose-ends-DONE-20261128.md`
// item 3). HotSpot's `Unsafe_DefineClass0` -> `JVM_DefineClass` defines in
// the loader argument. CratonVM's `defineClass0` native read raw field slot 6
// of the loader as its namespace id (a JDK field on the real layout), so
// under `--jdk-only` the class went to the application namespace: the
// application loader found it by name and its mirror did not name the user
// loader.
//
// NEEDS ARGS (the battery's no-argument run cannot export the package):
//   $J/javac --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -d out L5W96UnsafeDefineInLoader.java
//   $J/java --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -cp out L5W96UnsafeDefineInLoader   # HotSpot
//   $B --java-home $J/.. [--nojit] [--compatible] --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -cp out L5W96UnsafeDefineInLoader
//
// Expected block read from the JDK 25 sources (`Unsafe.defineClass`,
// `ClassLoader.loadClass` / `findLoadedClass`, `SystemDictionary::
// check_constraints` for the duplicate); the duplicate's exception TYPE only
// (its message names the loader with an identity hash). Not measured on
// HotSpot by this lane: confirm it on the host first.
//
// HOTSPOT_EXPECTED_BEGIN
// loader-is-l=true
// findLoaded-l=true
// forName-l=true
// forName-app=java.lang.ClassNotFoundException
// duplicate=java.lang.LinkageError
// second-loader distinct=true loader-is-l2=true
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import jdk.internal.misc.Unsafe;

public class L5W96UnsafeDefineInLoader {
    static final class L extends ClassLoader {
        L() {
            super(null);
        }

        Class<?> loaded(String name) {
            return findLoadedClass(name);
        }
    }

    /** `public class <name> extends Object {}` (version 49). */
    static byte[] emptyClass(String name) throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(49);
        out.writeShort(5);
        out.writeByte(1); out.writeUTF(name);
        out.writeByte(7); out.writeShort(1);
        out.writeByte(1); out.writeUTF("java/lang/Object");
        out.writeByte(7); out.writeShort(3);
        out.writeShort(0x21);
        out.writeShort(2);
        out.writeShort(4);
        out.writeShort(0);
        out.writeShort(0);
        out.writeShort(0);
        out.writeShort(0);
        out.flush();
        return bytes.toByteArray();
    }

    public static void main(String[] args) throws Exception {
        Unsafe u = Unsafe.getUnsafe();
        byte[] b = emptyClass("q/W96Gen");
        L l = new L();
        Class<?> c = u.defineClass("q.W96Gen", b, 0, b.length, l, null);
        System.out.println("loader-is-l=" + (c.getClassLoader() == l));
        System.out.println("findLoaded-l=" + (l.loaded("q.W96Gen") == c));
        System.out.println("forName-l=" + (Class.forName("q.W96Gen", false, l) == c));
        try {
            Class<?> app = Class.forName("q.W96Gen");
            System.out.println("forName-app=found same=" + (app == c));
        } catch (Throwable t) {
            System.out.println("forName-app=" + t.getClass().getName());
        }
        try {
            u.defineClass("q.W96Gen", b, 0, b.length, l, null);
            System.out.println("duplicate=defined");
        } catch (Throwable t) {
            System.out.println("duplicate=" + t.getClass().getName());
        }
        L l2 = new L();
        Class<?> c2 = u.defineClass("q.W96Gen", b, 0, b.length, l2, null);
        System.out.println("second-loader distinct=" + (c2 != c) + " loader-is-l2=" + (c2.getClassLoader() == l2));
    }
}
