// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 122, lane L5 -- the "ByteBuddy case" of
// `i55-L5-proposal-built-in-loaders-answer-packages-from-their-map`: a user
// loader's `getDefinedPackage` / `getDefinedPackages` / `Class.getPackage`
// before and after classes are INJECTED into it, the two ways ByteBuddy's
// `ClassInjector` does it without `--add-opens`: a `Lookup` on a class of the
// loader (`UsingLookup`: `privateLookupIn(anchor).defineClass`) and a loader
// that exposes `defineClass` (`ByteArrayClassLoader`-style). HotSpot answers
// every question from the loader's `packages` map: a package with a class
// file on the loader's path but no defined class is `null`, and the answer
// after a define is the same object `Class.getPackage()` returns. Under
// `--compatible` the VM's memo and class-file probe still answer for user
// loaders; under `--jdk-only` the JDK's bodies do.
//
// Self-contained: the class files are written to a temp directory at run time.
//
// Run: javac -d . L5W122InjectingLoaderPackages.java
//      java -cp . L5W122InjectingLoaderPackages                       (HotSpot)
//      cratonvm --jdk-only -cp . L5W122InjectingLoaderPackages
//      cratonvm --compatible -cp . L5W122InjectingLoaderPackages
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// before-any null
// file-only-package null
// after-load q same=true
// lookup-injected q same-package-object=true loader-is-ucl=true
// file-only-package-after null
// exposed-before null
// exposed-after s same=true
// exposed-listing [s]
// ucl-listing-has-q true has-r false
// sealed-q false spec-title null
// HOTSPOT_EXPECTED_END

import java.io.File;
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;

public class L5W122InjectingLoaderPackages {
    static byte[] plain(String name, String iface) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            if (iface != null) {
                cb.withInterfaceSymbols(ClassDesc.of(iface));
            }
            cb.withMethodBody("<init>", MethodTypeDesc.of(ConstantDescs.CD_void), ClassFile.ACC_PUBLIC, c -> c
                    .aload(0)
                    .invokespecial(ConstantDescs.CD_Object, "<init>", MethodTypeDesc.of(ConstantDescs.CD_void))
                    .return_());
        });
    }

    /** `q.Anchor` hands out a full-privilege lookup on itself. */
    static byte[] anchor() {
        ClassDesc lookup = ClassDesc.of("java.lang.invoke.MethodHandles$Lookup");
        return ClassFile.of().build(ClassDesc.of("q.Anchor"), cb -> cb
                .withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER)
                .withMethodBody("<init>", MethodTypeDesc.of(ConstantDescs.CD_void), ClassFile.ACC_PUBLIC, c -> c
                        .aload(0)
                        .invokespecial(ConstantDescs.CD_Object, "<init>", MethodTypeDesc.of(ConstantDescs.CD_void))
                        .return_())
                .withMethodBody("lookup", MethodTypeDesc.of(lookup), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, c -> c
                        .invokestatic(ClassDesc.of("java.lang.invoke.MethodHandles"), "lookup", MethodTypeDesc.of(lookup))
                        .areturn()));
    }

    static void write(Path root, String name, byte[] bytes) throws Exception {
        Path p = root.resolve(name.replace('.', '/') + ".class");
        Files.createDirectories(p.getParent());
        Files.write(p, bytes);
    }

    /** ByteBuddy's `ByteArrayClassLoader` shape: `defineClass` exposed. */
    static final class Exposing extends ClassLoader {
        Exposing() {
            super(L5W122InjectingLoaderPackages.class.getClassLoader());
        }

        Class<?> inject(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }

        Package defined(String pn) {
            return getDefinedPackage(pn);
        }
    }

    public static void main(String[] args) throws Exception {
        Path root = Files.createTempDirectory("l5w122");
        write(root, "q.Anchor", anchor());
        write(root, "r.OnlyFile", plain("r.OnlyFile", null));
        try (URLClassLoader ucl = new URLClassLoader(new URL[] {root.toUri().toURL()},
                ClassLoader.getPlatformClassLoader())) {
            System.out.println("before-any " + ucl.getDefinedPackage("q"));
            System.out.println("file-only-package " + ucl.getDefinedPackage("r"));
            Class<?> anchor = ucl.loadClass("q.Anchor");
            Package q = ucl.getDefinedPackage("q");
            System.out.println("after-load " + (q == null ? null : q.getName()) + " same=" + (q == anchor.getPackage()));

            MethodHandles.Lookup lk = (MethodHandles.Lookup) anchor.getMethod("lookup").invoke(null);
            Class<?> injected = lk.defineClass(plain("q.Injected", null));
            System.out.println("lookup-injected " + injected.getPackage().getName() + " same-package-object="
                    + (injected.getPackage() == ucl.getDefinedPackage("q")) + " loader-is-ucl="
                    + (injected.getClassLoader() == ucl));
            System.out.println("file-only-package-after " + ucl.getDefinedPackage("r"));

            Exposing ex = new Exposing();
            System.out.println("exposed-before " + ex.defined("s"));
            Class<?> s = ex.inject("s.Gen", plain("s.Gen", null));
            Package sp = ex.defined("s");
            System.out.println("exposed-after " + (sp == null ? null : sp.getName()) + " same=" + (sp == s.getPackage()));
            System.out.println("exposed-listing " + Arrays.stream(ex.getDefinedPackages()).map(Package::getName).sorted().toList());

            boolean hasQ = Arrays.stream(ucl.getDefinedPackages()).anyMatch(p -> p.getName().equals("q"));
            boolean hasR = Arrays.stream(ucl.getDefinedPackages()).anyMatch(p -> p.getName().equals("r"));
            System.out.println("ucl-listing-has-q " + hasQ + " has-r " + hasR);
            System.out.println("sealed-q " + q.isSealed() + " spec-title " + q.getSpecificationTitle());
        }
        for (File f : new File[] {root.resolve("q/Anchor.class").toFile(), root.resolve("r/OnlyFile.class").toFile(),
                root.resolve("q").toFile(), root.resolve("r").toFile(), root.toFile()}) {
            f.delete();
        }
    }
}
