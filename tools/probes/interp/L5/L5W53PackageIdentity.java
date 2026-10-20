// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2, wave 53, lane L5 -- `Package` identity. HotSpot keeps
// ONE `Package` per (loader, package name) in the loader's `packages` map:
// `Class.getPackage()` answers that object every time, `Package` inherits
// `Object.equals` (identity), and two loaders' same-named packages are two
// unequal objects. CratonVM's `Class.getPackage` native answered a `Package`
// of its own memo even when the loader's map held the JDK's (`defined`: the
// one `URLClassLoader.defineClass` defined with the jar's manifest), and its
// by-NAME `Package.equals` (`native_package_equals`, registered when
// `getPackage` minted a fresh object per call) made two loaders' packages
// equal (`cross`). Both fixed in wave 53 (lane L5): `getPackage` answers a
// user loader's map entry first, and `equals` needs the same module too.
//
// Rows, first (a cold VM) and again last, on fresh loaders over one generated
// jar (`l5w53q/A.class`):
//
//   same     `A.class.getPackage() == A.class.getPackage()`, and `==` the
//            loader's `getDefinedPackage`.
//   cross    two loaders' `l5w53q` packages: `equals`, and equal hash codes
//            (the JDK's `Package.hashCode` is the name's).
//   jdk      `String.class.getPackage() == Object.class.getPackage()` (one
//            boot package, `java.lang`).
//
// Run (no setup):
//   javac -d out L5W53PackageIdentity.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W53PackageIdentity
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   same-1=twice=true defined=true
//   cross-1=equals=false hash=true
//   jdk-1=true
//   same-2=twice=true defined=true
//   cross-2=equals=false hash=true
//   jdk-2=true

import java.io.OutputStream;
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.jar.JarEntry;
import java.util.jar.JarOutputStream;

public class L5W53PackageIdentity {
    static Path jar() throws Exception {
        Path jar = Files.createTempDirectory("l5w53q").resolve("q.jar");
        byte[] a = ClassFile.of().build(ClassDesc.of("l5w53q.A"), cb -> cb
                .withFlags(ClassFile.ACC_PUBLIC)
                .withSuperclass(ConstantDescs.CD_Object));
        try (OutputStream os = Files.newOutputStream(jar); JarOutputStream jos = new JarOutputStream(os)) {
            jos.putNextEntry(new JarEntry("l5w53q/A.class"));
            jos.write(a);
            jos.closeEntry();
        }
        return jar;
    }

    static void round(int r, Path jar) throws Exception {
        URL[] urls = {jar.toUri().toURL()};
        try (URLClassLoader u1 = new URLClassLoader(urls, null); URLClassLoader u2 = new URLClassLoader(urls, null)) {
            Class<?> a1 = u1.loadClass("l5w53q.A");
            Class<?> a2 = u2.loadClass("l5w53q.A");
            Package p1 = a1.getPackage();
            System.out.println("same-" + r + "=twice=" + (p1 == a1.getPackage())
                    + " defined=" + (p1 == u1.getDefinedPackage("l5w53q")));
            Package p2 = a2.getPackage();
            System.out.println("cross-" + r + "=equals=" + p1.equals(p2) + " hash=" + (p1.hashCode() == p2.hashCode()));
        }
        System.out.println("jdk-" + r + "=" + (String.class.getPackage() == Object.class.getPackage()));
    }

    public static void main(String[] args) throws Exception {
        Path jar = jar();
        for (int r = 1; r <= 2; r++) {
            round(r, jar);
        }
    }
}
