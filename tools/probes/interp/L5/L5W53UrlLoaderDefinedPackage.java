// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2, wave 53, lane L5 -- `ClassLoader.getDefinedPackage`
// of a `URLClassLoader` whose own bodies run (`--jdk-only`, and
// `--compatible` with the `jdk-url-class-loaders` HotSpot-alignment row)
// answers from the loader's `packages` map only, as the JDK's body does.
// CratonVM's native fabricated a `Package` for any package whose class files
// the loader's URLs hold (`classloader::package_class_files_visible_to_loader`,
// a glob over every entry of every jar of the loader, per miss), BEFORE the
// loader defined anything there. Spring Boot's `JarUrlClassLoader
// .definePackageIfNecessary` asks exactly that before every class load, so the
// fat jar paid a whole-class-path scan per new package (and per absent-class
// probe), and the fabricated answer made it -- and `URLClassLoader.defineClass`'s
// own `getAndVerifyPackage` -- skip `definePackage(name, manifest, url)`: the
// package lost its manifest attributes.
//
// Rows, first (a cold VM) and again last, on fresh loaders over a generated
// jar (`l5w53p/A.class`, manifest `Implementation-Version: 1.2.3`):
//
//   before   `getDefinedPackage("l5w53p")` before any class is loaded: null.
//   after    after `loadClass("l5w53p.A")`: the class's package's
//            implementation version, and whether `getDefinedPackage` now
//            answers that same `Package`.
//
// Run (no setup):
//   javac -d out L5W53UrlLoaderDefinedPackage.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W53UrlLoaderDefinedPackage
//
// Positive control (`--compatible`): `CRATONVM_COMPAT_ALIGN=-jdk-url-class-loaders`
// keeps the stand-in loader and the scan.
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   before-1=null
//   after-1=version=1.2.3 same=true
//   before-2=null
//   after-2=version=1.2.3 same=true

import java.io.OutputStream;
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.jar.Attributes;
import java.util.jar.JarEntry;
import java.util.jar.JarOutputStream;
import java.util.jar.Manifest;

public class L5W53UrlLoaderDefinedPackage {
    static Path jar() throws Exception {
        Path dir = Files.createTempDirectory("l5w53p");
        Path jar = dir.resolve("p.jar");
        Manifest m = new Manifest();
        m.getMainAttributes().put(Attributes.Name.MANIFEST_VERSION, "1.0");
        m.getMainAttributes().put(Attributes.Name.IMPLEMENTATION_VERSION, "1.2.3");
        byte[] a = ClassFile.of().build(ClassDesc.of("l5w53p.A"), cb -> cb
                .withFlags(ClassFile.ACC_PUBLIC)
                .withSuperclass(ConstantDescs.CD_Object));
        try (OutputStream os = Files.newOutputStream(jar); JarOutputStream jos = new JarOutputStream(os, m)) {
            jos.putNextEntry(new JarEntry("l5w53p/"));
            jos.closeEntry();
            jos.putNextEntry(new JarEntry("l5w53p/A.class"));
            jos.write(a);
            jos.closeEntry();
        }
        return jar;
    }

    static void round(int r, Path jar) throws Exception {
        try (URLClassLoader u = new URLClassLoader(new URL[] {jar.toUri().toURL()}) {}) {
            Package before = u.getDefinedPackage("l5w53p");
            System.out.println("before-" + r + "=" + (before == null ? "null" : "package"));
            Class<?> a = u.loadClass("l5w53p.A");
            Package p = a.getPackage();
            System.out.println("after-" + r + "=version=" + (p == null ? "nopkg" : p.getImplementationVersion())
                    + " same=" + (u.getDefinedPackage("l5w53p") == p));
        }
    }

    public static void main(String[] args) throws Exception {
        Path jar = jar();
        for (int r = 1; r <= 2; r++) {
            round(r, jar);
        }
    }
}
