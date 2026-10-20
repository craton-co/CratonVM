// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 106, lane L3 (hunt): `Class.getClassFileVersion0`
// is `(minor << 16) | major` on HotSpot (`JVM_GetClassFileVersion`), and a
// primitive's (and `void`'s) is `JVM_CLASSFILE_MAJOR_VERSION`, 69 on JDK 25,
// with minor 0. `ReflectionFactory.classFileFormatVersion` -- behind
// `Class.accessFlags()`, `Method.accessFlags()`, ... -- reads the minor to
// spot a preview or a JDK 1.0 (45.0-45.2) class file. CratonVM answered the
// major alone (45.3 read as 45) and 65 for a primitive.
//
// Read through `JavaLangAccess.classFileVersion(Class)` (what the JDK's own
// callers use); printed in hex. The 45.3 and 52.0 classes are built with the
// ClassFile API and defined by this class's lookup.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3; derived from jvm.cpp's
// JVM_GetClassFileVersion and Class.getClassFileVersion; the probe compiled
// by javac 25 without --release)
// int: 45
// void: 45
// int[][]: 45
// String: 45
// Object[]: 45
// probe: 45
// nested: 45
// v52.0: 34
// v45.3: 3002d
// v45.3[]: 3002d
// v52-access-flags: [PUBLIC, SUPER]
// HOTSPOT_EXPECTED_END
//
// SETUP: `javac L3W106ClassFileVersion.java`, then
//   java|cratonvm [--compatible] [--nojit] --add-exports java.base/jdk.internal.access=ALL-UNNAMED
//     -cp . L3W106ClassFileVersion
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.Method;

public class L3W106ClassFileVersion {
    static class Nested {
    }

    static Object jla;
    static Method classFileVersion;

    static String version(Class<?> c) {
        try {
            return Integer.toHexString((Integer) classFileVersion.invoke(jla, c));
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    static Class<?> define(String name, int major, int minor) throws Throwable {
        byte[] bytes = ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withVersion(major, minor);
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withSuperclass(ConstantDescs.CD_Object);
        });
        return MethodHandles.lookup().defineClass(bytes);
    }

    public static void main(String[] args) throws Throwable {
        Class<?> ss = Class.forName("jdk.internal.access.SharedSecrets");
        jla = ss.getMethod("getJavaLangAccess").invoke(null);
        classFileVersion = Class.forName("jdk.internal.access.JavaLangAccess")
                .getMethod("classFileVersion", Class.class);
        System.out.println("int: " + version(int.class));
        System.out.println("void: " + version(void.class));
        System.out.println("int[][]: " + version(int[][].class));
        System.out.println("String: " + version(String.class));
        System.out.println("Object[]: " + version(Object[].class));
        System.out.println("probe: " + version(L3W106ClassFileVersion.class));
        System.out.println("nested: " + version(Nested.class));
        Class<?> v52 = define("L3W106V52", 52, 0);
        System.out.println("v52.0: " + version(v52));
        Class<?> v45 = define("L3W106V45", 45, 3);
        System.out.println("v45.3: " + version(v45));
        System.out.println("v45.3[]: " + version(v45.arrayType()));
        System.out.println("v52-access-flags: " + v52.accessFlags());
    }
}
