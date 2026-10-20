// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 94, lane L5 -- every door that hands out a class
// of a package sealed by ANOTHER class-path JAR refuses it with the
// `SecurityException` HotSpot raises at the class's define
// (`BuiltinClassLoader.defineClass(cn, Resource)` -> `defineOrCheckPackage` ->
// `getAndVerifyPackage`), not only `Class.forName` (wave 92,
// `L5W92ForNameSealedNoInit`). The VM defines class-path classes itself and ran
// the JDK's package step at link and in `forName(n, false, app)` only, so a
// constant-pool resolution that does not link (`ldc B.class`, `checkcast`,
// `instanceof`, `anewarray`) and the `--compatible` `loadClass` natives handed
// out `B` (`i92-L5-unlinked-class-path-class-doors-skip-the-package-step`).
//
// `setup <dir>` writes three JARs:
//   a.jar  manifest `Sealed: true`, class `l5w94seal.A` (public, no members)
//   b.jar  class `l5w94seal.B`, no manifest
//   c.jar  class `l5w94use.Use`, built with java.lang.classfile: static
//          `ldc()` (`ldc l5w94seal/B`), `cast(Object)` (`checkcast B`),
//          `castArray(Object)` (`checkcast [Ll5w94seal/B;`), `inst(Object)`
//          (`instanceof B`) and `arr()` (`iconst_0; anewarray B`)
//
// The package is sealed by loading `A` first. Each door is then asked twice
// (a refusal is not a `LinkageError`, so it is not recorded: JVMS 5.4.3 does
// not apply, and HotSpot asks the loader again). `cast` / `inst` get a
// non-null `String`, so the instruction resolves its class (a null operand
// would not).
//
// SETUP and run (one directory; `;` for `:` on Windows):
//   javac -d out L5W94SealedUnlinkedDoors.java
//   java -cp out L5W94SealedUnlinkedDoors setup .
//   java -cp out:a.jar:b.jar:c.jar L5W94SealedUnlinkedDoors
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out:a.jar:b.jar:c.jar L5W94SealedUnlinkedDoors
//   (`CRATONVM_UNLINKED_DOORS_CHECK_PACKAGE=0` is the kill switch: the
//   constant-pool rows then print the class / `false` / `[Ll5w94seal.B;`.)
//
// HOTSPOT_EXPECTED (derived from the JDK 25 sources, not run: the class `B` is
// never defined, because `getAndVerifyPackage` throws `new
// SecurityException("sealing violation: package " + pn + " is sealed")` on
// every define attempt; a resolution that calls the application loader's
// `loadClass` propagates that exception unchanged; `Method.invoke` wraps it in
// `InvocationTargetException`, whose cause is printed):
//   a=l5w94seal.A
//   ldc-1=java.lang.SecurityException msg=sealing violation: package l5w94seal is sealed
//   cast-1=java.lang.SecurityException msg=sealing violation: package l5w94seal is sealed
//   cast-array-1=java.lang.SecurityException msg=sealing violation: package l5w94seal is sealed
//   instanceof-1=java.lang.SecurityException msg=sealing violation: package l5w94seal is sealed
//   anewarray-1=java.lang.SecurityException msg=sealing violation: package l5w94seal is sealed
//   load-1=java.lang.SecurityException msg=sealing violation: package l5w94seal is sealed
//   ldc-2=java.lang.SecurityException msg=sealing violation: package l5w94seal is sealed
//   cast-2=java.lang.SecurityException msg=sealing violation: package l5w94seal is sealed
//   cast-array-2=java.lang.SecurityException msg=sealing violation: package l5w94seal is sealed
//   instanceof-2=java.lang.SecurityException msg=sealing violation: package l5w94seal is sealed
//   anewarray-2=java.lang.SecurityException msg=sealing violation: package l5w94seal is sealed
//   load-2=java.lang.SecurityException msg=sealing violation: package l5w94seal is sealed
//   sealed=true

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.concurrent.Callable;
import java.util.jar.Attributes;
import java.util.jar.JarEntry;
import java.util.jar.JarOutputStream;
import java.util.jar.Manifest;

public class L5W94SealedUnlinkedDoors {
    static String attempt(Callable<Object> c) {
        try {
            Object v = c.call();
            return v instanceof Class<?> k ? k.getName() : String.valueOf(v);
        } catch (Throwable t) {
            if (t instanceof InvocationTargetException ite && ite.getCause() != null) {
                t = ite.getCause();
            }
            return t.getClass().getName() + " msg=" + t.getMessage();
        }
    }

    /** A public class `internal` extending Object, with no members. */
    static byte[] emptyClass(String internal) throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(52);
        out.writeShort(5);
        out.writeByte(7);
        out.writeShort(2);
        out.writeByte(1);
        out.writeUTF(internal);
        out.writeByte(7);
        out.writeShort(4);
        out.writeByte(1);
        out.writeUTF("java/lang/Object");
        out.writeShort(0x0021);
        out.writeShort(1);
        out.writeShort(3);
        out.writeShort(0);
        out.writeShort(0);
        out.writeShort(0);
        out.writeShort(0);
        out.flush();
        return bytes.toByteArray();
    }

    static byte[] useClass() {
        ClassDesc b = ClassDesc.of("l5w94seal.B");
        int flags = ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC;
        MethodTypeDesc toObject = MethodTypeDesc.of(ConstantDescs.CD_Object);
        MethodTypeDesc objToObject = MethodTypeDesc.of(ConstantDescs.CD_Object, ConstantDescs.CD_Object);
        return ClassFile.of().build(ClassDesc.of("l5w94use.Use"), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("ldc", toObject, flags, code -> code.ldc(b).areturn());
            cb.withMethodBody("cast", objToObject, flags,
                    code -> code.aload(0).checkcast(b).areturn());
            cb.withMethodBody("castArray", objToObject, flags,
                    code -> code.aload(0).checkcast(b.arrayType()).areturn());
            cb.withMethodBody("inst",
                    MethodTypeDesc.of(ConstantDescs.CD_boolean, ConstantDescs.CD_Object), flags,
                    code -> code.aload(0).instanceOf(b).ireturn());
            cb.withMethodBody("arr", toObject, flags,
                    code -> code.iconst_0().anewarray(b).areturn());
        });
    }

    static void jar(File file, Manifest man, String internal, byte[] bytes) throws IOException {
        try (JarOutputStream out = man == null
                ? new JarOutputStream(new FileOutputStream(file))
                : new JarOutputStream(new FileOutputStream(file), man)) {
            out.putNextEntry(new JarEntry(internal + ".class"));
            out.write(bytes);
            out.closeEntry();
        }
    }

    static void setup(File dir) throws IOException {
        Manifest man = new Manifest();
        man.getMainAttributes().put(Attributes.Name.MANIFEST_VERSION, "1.0");
        man.getMainAttributes().put(Attributes.Name.SEALED, "true");
        jar(new File(dir, "a.jar"), man, "l5w94seal/A", emptyClass("l5w94seal/A"));
        jar(new File(dir, "b.jar"), null, "l5w94seal/B", emptyClass("l5w94seal/B"));
        jar(new File(dir, "c.jar"), null, "l5w94use/Use", useClass());
        System.out.println("wrote a.jar, b.jar and c.jar in " + dir);
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("setup")) {
            setup(new File(args[1]));
            return;
        }
        ClassLoader app = ClassLoader.getSystemClassLoader();
        System.out.println("a=" + attempt(() -> Class.forName("l5w94seal.A", false, app)));
        Class<?> use = Class.forName("l5w94use.Use", true, app);
        Method ldc = use.getMethod("ldc");
        Method cast = use.getMethod("cast", Object.class);
        Method castArray = use.getMethod("castArray", Object.class);
        Method inst = use.getMethod("inst", Object.class);
        Method arr = use.getMethod("arr");
        for (String tag : new String[] {"1", "2"}) {
            System.out.println("ldc-" + tag + "=" + attempt(() -> ldc.invoke(null)));
            System.out.println("cast-" + tag + "=" + attempt(() -> cast.invoke(null, "x")));
            System.out.println("cast-array-" + tag + "=" + attempt(() -> castArray.invoke(null, "x")));
            System.out.println("instanceof-" + tag + "=" + attempt(() -> inst.invoke(null, "x")));
            System.out.println("anewarray-" + tag + "=" + attempt(() -> {
                Object a = arr.invoke(null);
                return a.getClass();
            }));
            System.out.println("load-" + tag + "=" + attempt(() -> app.loadClass("l5w94seal.B")));
        }
        System.out.println("sealed=" + attempt(() -> {
            Package p = app.getDefinedPackage("l5w94seal");
            return p == null ? "null" : String.valueOf(p.isSealed());
        }));
    }
}
