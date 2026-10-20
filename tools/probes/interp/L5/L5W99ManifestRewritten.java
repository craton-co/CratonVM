// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4, wave 99, lane L5 --
// `docs/internal/fixed-bugs/interpreter-i98-orch-manifest-caches-are-process-wide-and-never-invalidated-FIXED-20261015.md`.
// `native-builtins` `lang_class.rs` kept the jar manifests `Class.getPackage()`
// reads in three process globals keyed by path only (`plain_manifest_cache`,
// `nested_manifest_cache`, `exploded_manifest_cache`): a jar rewritten on disk
// kept its first manifest for the life of the process. Wave 99 moved them per
// VM with a (length, mtime) stamp per row.
//
// Which Java calls reach those caches: `--compatible`'s `Class.getPackage`
// carrier (`native_class_get_package`) for a class whose loader's `packages`
// map holds no `Package` -- in particular a user loader's PLAIN `defineClass`
// with a jar `CodeSource`. There the carrier read the jar's manifest (the
// cached path) where HotSpot's `ClassLoader.definePackage(Class)` answers a
// `Package` with NULL_VERSION_INFO: the `define` rows print null on HotSpot
// (wave 99 stops the carrier reading the manifest for a user loader's class;
// by code reading, CratonVM `--compatible` printed `1` in BOTH define rows
// before -- the jar's version, then the stale path-keyed cache row). `--jdk-only` runs the JDK body. The `url` rows
// (`URLClassLoader`, whose `defineClass` runs `definePackage(name, manifest,
// url)`) are the control HotSpot defines: a NEW loader over the rewritten
// jar reads the new manifest.
//
// The jar is rewritten IN PLACE under the same name (the case a path-only key
// gets wrong); the second manifest has a different length, so the stamp moves
// whatever the file system's mtime granularity. Every loader is closed and
// every `JarFile` closed before the rewrite, so the rewrite also succeeds on
// Windows (HotSpot holds no handle on the jar then).
//
// Run (no setup):
//   javac -d out L5W99ManifestRewritten.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W99ManifestRewritten
//
// Expected HotSpot 25.0.3 output (`java -cp out L5W99ManifestRewritten`; compare verbatim):
//   url-1=w99p:1 w99r:1
//   define-1=null
//   url-2=w99p:2.0 w99r:main-2
//   define-2=null

import java.io.OutputStream;
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.CodeSource;
import java.security.ProtectionDomain;
import java.security.cert.Certificate;
import java.util.jar.Attributes;
import java.util.jar.JarEntry;
import java.util.jar.JarFile;
import java.util.jar.JarOutputStream;
import java.util.jar.Manifest;

public class L5W99ManifestRewritten {
    static byte[] cls(String name) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> cb
                .withFlags(ClassFile.ACC_PUBLIC)
                .withSuperclass(ConstantDescs.CD_Object));
    }

    static void entry(JarOutputStream jos, String name, byte[] bytes) throws Exception {
        jos.putNextEntry(new JarEntry(name));
        if (bytes != null) {
            jos.write(bytes);
        }
        jos.closeEntry();
    }

    // Round 1: main `Implementation-Version: 1`. Round 2: main `main-2`, and a
    // `Name: w99p/` section with `2.0` (a package section beats the main one).
    static void writeJar(Path jar, int round) throws Exception {
        Manifest m = new Manifest();
        m.getMainAttributes().put(Attributes.Name.MANIFEST_VERSION, "1.0");
        m.getMainAttributes().put(Attributes.Name.IMPLEMENTATION_VERSION, round == 1 ? "1" : "main-2");
        if (round == 2) {
            Attributes section = new Attributes();
            section.put(Attributes.Name.IMPLEMENTATION_VERSION, "2.0");
            m.getEntries().put("w99p/", section);
        }
        try (OutputStream os = Files.newOutputStream(jar); JarOutputStream jos = new JarOutputStream(os, m)) {
            entry(jos, "w99p/", null);
            entry(jos, "w99p/A.class", cls("w99p.A"));
            entry(jos, "w99q/", null);
            entry(jos, "w99q/B.class", cls("w99q.B"));
            entry(jos, "w99r/", null);
            entry(jos, "w99r/C.class", cls("w99r.C"));
        }
    }

    static final class DefineLoader extends ClassLoader {
        DefineLoader() {
            super(null);
        }

        Class<?> define(String name, byte[] b, URL url) {
            CodeSource cs = new CodeSource(url, (Certificate[]) null);
            return defineClass(name, b, 0, b.length, new ProtectionDomain(cs, null));
        }
    }

    static String version(Class<?> c) {
        Package p = c.getPackage();
        return p == null ? "nopkg" : String.valueOf(p.getImplementationVersion());
    }

    static void round(int r, Path jar) throws Exception {
        URL url = jar.toUri().toURL();
        try (URLClassLoader u = new URLClassLoader(new URL[] {url}, null)) {
            Class<?> a = u.loadClass("w99p.A");
            Class<?> c = u.loadClass("w99r.C");
            System.out.println("url-" + r + "=w99p:" + version(a) + " w99r:" + version(c));
        }
        byte[] b;
        try (JarFile jf = new JarFile(jar.toFile())) {
            b = jf.getInputStream(jf.getJarEntry("w99q/B.class")).readAllBytes();
        }
        Class<?> bc = new DefineLoader().define("w99q.B", b, url);
        System.out.println("define-" + r + "=" + version(bc));
    }

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("l5w99m");
        Path jar = dir.resolve("w99.jar");
        writeJar(jar, 1);
        round(1, jar);
        writeJar(jar, 2);
        round(2, jar);
    }
}
