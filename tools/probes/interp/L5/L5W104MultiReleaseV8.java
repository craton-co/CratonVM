// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, wave 104, lane L5
// (docs/internal/fixed-bugs/interpreter-i100-L5-multi-release-jars-read-at-version-25-whatever-jdk-boots-FIXED-20261016.md,
// residue "META-INF/versions/8 is not served").
//
// JDK 25's JarFile.getVersionedEntry walks the versions present in the jar
// from the runtime version DOWN TO 8 (BASE_VERSION_FEATURE), so a
// META-INF/versions/8/ entry is served by a multi-release jar; versions below
// 8, zero-padded version directories and META-INF/ names are never versioned.
// CratonVM's own class-path reader (the --compatible URLClassLoader stand-in,
// the launch class path) stopped at 9.
//
// Self-contained: writes a multi-release jar to a temp directory, then reads
// it through a URLClassLoader (parent: the platform loader, so the probe's own
// class path is not consulted) and through java.util.jar.JarFile.
//
// Run: java L5W104MultiReleaseV8.java (or javac + java), and
//      cratonvm --java-home <jdk25> [--compatible] -cp <dir> L5W104MultiReleaseV8
//
// Expected (HotSpot 25, both modes):
//   loader r.txt=v8
//   loader s.txt=base
//   loader only8.txt=v8
//   loader only5.txt=null
//   loader padded.txt=null
//   loader META-INF/m.txt=base
//   loader class Eight=eight
//   jar r.txt=v8 real=META-INF/versions/8/r.txt
//   jar s.txt=base real=s.txt
//   jar only8.txt=v8 real=META-INF/versions/8/only8.txt
//   jar only5.txt=null
//   jar padded.txt=null
//   base r.txt=base
// Before wave 104 (--compatible): `loader r.txt=base`, `loader only8.txt=null`,
// `loader class Eight=ClassNotFoundException`.

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
import java.util.zip.ZipEntry;
import java.util.zip.ZipFile;

public class L5W104MultiReleaseV8 {
    /** Stored only under META-INF/versions/8/ in the jar. */
    public static class Eight {
        @Override
        public String toString() {
            return "eight";
        }
    }

    static byte[] readAll(InputStream in) throws Exception {
        try (InputStream i = in) {
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            i.transferTo(out);
            return out.toByteArray();
        }
    }

    static void put(JarOutputStream out, String name, byte[] body) throws Exception {
        out.putNextEntry(new JarEntry(name));
        out.write(body);
        out.closeEntry();
    }

    static byte[] text(String s) {
        return s.getBytes(StandardCharsets.UTF_8);
    }

    static String content(InputStream in) throws Exception {
        return in == null ? "null" : new String(readAll(in), StandardCharsets.UTF_8);
    }

    public static void main(String[] args) throws Exception {
        String eightName = Eight.class.getName().replace('.', '/') + ".class";
        byte[] eightBytes = readAll(L5W104MultiReleaseV8.class.getClassLoader()
                .getResourceAsStream(eightName));

        File dir = java.nio.file.Files.createTempDirectory("l5w104mr8").toFile();
        File jar = new File(dir, "mr8.jar");
        Manifest mf = new Manifest();
        mf.getMainAttributes().put(Attributes.Name.MANIFEST_VERSION, "1.0");
        mf.getMainAttributes().put(new Attributes.Name("Multi-Release"), "true");
        try (JarOutputStream out = new JarOutputStream(new FileOutputStream(jar), mf)) {
            put(out, "r.txt", text("base"));
            put(out, "META-INF/versions/8/r.txt", text("v8"));
            put(out, "s.txt", text("base"));
            put(out, "META-INF/versions/7/s.txt", text("v7"));
            put(out, "META-INF/versions/8/only8.txt", text("v8"));
            put(out, "META-INF/versions/5/only5.txt", text("v5"));
            put(out, "META-INF/versions/09/padded.txt", text("v09"));
            put(out, "META-INF/m.txt", text("base"));
            put(out, "META-INF/versions/9/META-INF/m.txt", text("v9"));
            put(out, "META-INF/versions/8/" + eightName, eightBytes);
        }

        try (URLClassLoader loader = new URLClassLoader(new URL[] {jar.toURI().toURL()},
                ClassLoader.getPlatformClassLoader())) {
            for (String name : new String[] {"r.txt", "s.txt", "only8.txt", "only5.txt",
                    "padded.txt", "META-INF/m.txt"}) {
                System.out.println("loader " + name + "=" + content(loader.getResourceAsStream(name)));
            }
            String cls;
            try {
                cls = loader.loadClass(Eight.class.getName()).getDeclaredConstructor()
                        .newInstance().toString();
            } catch (ClassNotFoundException e) {
                cls = "ClassNotFoundException";
            }
            System.out.println("loader class Eight=" + cls);
        }

        try (JarFile jf = new JarFile(jar, true, ZipFile.OPEN_READ, JarFile.runtimeVersion())) {
            for (String name : new String[] {"r.txt", "s.txt", "only8.txt", "only5.txt",
                    "padded.txt"}) {
                JarEntry e = jf.getJarEntry(name);
                if (e == null) {
                    System.out.println("jar " + name + "=null");
                } else {
                    System.out.println("jar " + name + "=" + content(jf.getInputStream(e))
                            + " real=" + e.getRealName());
                }
            }
        }
        try (JarFile jf = new JarFile(jar)) {
            ZipEntry e = jf.getEntry("r.txt");
            System.out.println("base r.txt=" + content(jf.getInputStream(e)));
        }

        for (File f : dir.listFiles()) {
            f.delete();
        }
        dir.delete();
    }
}
