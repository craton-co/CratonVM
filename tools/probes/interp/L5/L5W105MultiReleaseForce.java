// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, wave 105, lane L5
// (docs/internal/fixed-bugs/interpreter-i100-L5-multi-release-jars-read-at-version-25-whatever-jdk-boots-FIXED-20261016.md,
// residue "-Djdk.util.jar.enableMultiRelease=force is not modelled").
//
// JDK 25's JarFile.<clinit> reads jdk.util.jar.enableMultiRelease: "false"
// turns multi-release off, "force" sets MULTI_RELEASE_FORCED, anything else is
// the default. FORCED does NOT make a jar without `Multi-Release: true`
// multi-release (isMultiRelease() still reads the manifest attribute); it only
// makes the JarFile(File, boolean, int, Runtime.Version) constructor use
// RUNTIME_VERSION whatever version it is given. The class path opens its jars
// at JarFile.runtimeVersion() anyway, so "force" reads the class path exactly
// as the default does: CratonVM's own class-path reader needs no "force" arm
// (ClassPath::jar_runtime_version treats it as the default), and the JDK's own
// JarFile bytecode answers the explicit-version rows in both modes.
//
// Self-contained: writes a multi-release jar (mr.jar) and the same entries
// without the manifest attribute (plain.jar) to a temp directory.
//
// Run (each of the three, HotSpot 25 and cratonvm --java-home <jdk25>, both modes):
//   java L5W105MultiReleaseForce.java
//   java -Djdk.util.jar.enableMultiRelease=force L5W105MultiReleaseForce.java
//   java -Djdk.util.jar.enableMultiRelease=false L5W105MultiReleaseForce.java
//   cratonvm --java-home <jdk25> [--compatible] [-D...] -cp <dir> L5W105MultiReleaseForce
//
// Expected, default (no property):
//   prop=null
//   mr isMultiRelease=true
//   mr v11 r.txt=11 version=11
//   mr runtime r.txt=25 version=25
//   plain isMultiRelease=false
//   plain v11 r.txt=base version=8
//   loader mr r.txt=25
//   loader plain r.txt=base
// Expected, -Djdk.util.jar.enableMultiRelease=force: the same except
//   prop=force
//   mr v11 r.txt=25 version=25
// Expected, -Djdk.util.jar.enableMultiRelease=false:
//   prop=false
//   mr isMultiRelease=false
//   mr v11 r.txt=base version=8
//   mr runtime r.txt=base version=8
//   plain isMultiRelease=false
//   plain v11 r.txt=base version=8
//   loader mr r.txt=base
//   loader plain r.txt=base

import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.charset.StandardCharsets;
import java.util.jar.Attributes;
import java.util.jar.JarEntry;
import java.util.jar.JarFile;
import java.util.jar.JarOutputStream;
import java.util.jar.Manifest;
import java.util.zip.ZipFile;

public class L5W105MultiReleaseForce {
    static String readAll(InputStream in) throws Exception {
        if (in == null) {
            return "null";
        }
        try (InputStream i = in) {
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            i.transferTo(out);
            return out.toString(StandardCharsets.UTF_8);
        }
    }

    static void put(JarOutputStream out, String name, String body) throws Exception {
        out.putNextEntry(new JarEntry(name));
        out.write(body.getBytes(StandardCharsets.UTF_8));
        out.closeEntry();
    }

    static File write(File dir, String name, boolean multiRelease) throws Exception {
        Manifest man = new Manifest();
        man.getMainAttributes().put(Attributes.Name.MANIFEST_VERSION, "1.0");
        if (multiRelease) {
            man.getMainAttributes().put(Attributes.Name.MULTI_RELEASE, "true");
        }
        File f = new File(dir, name);
        try (JarOutputStream out = new JarOutputStream(new FileOutputStream(f), man)) {
            put(out, "r.txt", "base");
            put(out, "META-INF/versions/11/r.txt", "11");
            put(out, "META-INF/versions/21/r.txt", "21");
            put(out, "META-INF/versions/25/r.txt", "25");
        }
        return f;
    }

    static String entry(JarFile jf) throws Exception {
        JarEntry e = jf.getJarEntry("r.txt");
        return e == null ? "null" : readAll(jf.getInputStream(e));
    }

    static void jarRows(String label, File f) throws Exception {
        try (JarFile jf = new JarFile(f, true, ZipFile.OPEN_READ, JarFile.runtimeVersion())) {
            System.out.println(label + " isMultiRelease=" + jf.isMultiRelease());
        }
        try (JarFile jf = new JarFile(f, true, ZipFile.OPEN_READ, Runtime.Version.parse("11"))) {
            System.out.println(label + " v11 r.txt=" + entry(jf)
                    + " version=" + jf.getVersion().feature());
        }
    }

    public static void main(String[] args) throws Exception {
        File dir = java.nio.file.Files.createTempDirectory("l5w105mrf").toFile();
        File mr = write(dir, "mr.jar", true);
        File plain = write(dir, "plain.jar", false);
        System.out.println("prop=" + System.getProperty("jdk.util.jar.enableMultiRelease"));
        jarRows("mr", mr);
        try (JarFile jf = new JarFile(mr, true, ZipFile.OPEN_READ, JarFile.runtimeVersion())) {
            System.out.println("mr runtime r.txt=" + entry(jf)
                    + " version=" + jf.getVersion().feature());
        }
        jarRows("plain", plain);
        ClassLoader platform = ClassLoader.getPlatformClassLoader();
        try (URLClassLoader l = new URLClassLoader(new URL[] {mr.toURI().toURL()}, platform)) {
            System.out.println("loader mr r.txt=" + readAll(l.getResourceAsStream("r.txt")));
        }
        try (URLClassLoader l = new URLClassLoader(new URL[] {plain.toURI().toURL()}, platform)) {
            System.out.println("loader plain r.txt=" + readAll(l.getResourceAsStream("r.txt")));
        }
        mr.delete();
        plain.delete();
        dir.delete();
    }
}
