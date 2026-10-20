// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 57, lane L5 -- `Module.getResourceAsStream` of a
// USER loader's unnamed module asks that loader (`ClassLoader.findResource
// (null, name)`, so the loader's own `findResource`, no parent), not the
// application class path. The probe writes a jar holding `a.txt` and a copy
// of `Inner`, and defines `Inner` from it through a `URLClassLoader` whose
// parent is the boot loader.
//
// Rows (the first call, and again last):
//   own-*   `Inner`'s module `getResourceAsStream("a.txt")`: the jar's entry
//   app-*   the same module asked for this probe's own class file, which only
//           the application class path holds: null
//   named-* `Object.class.getModule().getResourceAsStream("java/lang/Object.class")`
//           (a named module's own class file: served)
//
// Run (no setup):
//   javac -d out L5W57UserModuleResources.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W57UserModuleResources
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   own-1=hello
//   app-1=null
//   named-1=cafebabe
//   own-2=hello
//   app-2=null
//   named-2=cafebabe
//
// CratonVM before wave 57 (debug build, Windows, measured): `--jdk-only`
// matched; `--compatible` printed `own-N=null` and `app-N=cafebabe` (the
// `Module.getResourceAsStream` stand-in scanned the application class path).

import java.io.File;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.jar.JarOutputStream;
import java.util.zip.ZipEntry;

public class L5W57UserModuleResources {
    public static class Inner {
    }

    static String attempt(java.util.concurrent.Callable<Object> c) {
        try {
            return String.valueOf(c.call());
        } catch (Throwable t) {
            return t.getClass().getName() + " msg=" + t.getMessage();
        }
    }

    static String text(InputStream in) throws Exception {
        if (in == null) {
            return "null";
        }
        try (in) {
            return new String(in.readNBytes(5), StandardCharsets.US_ASCII);
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

    public static void main(String[] args) throws Exception {
        File jar = new File(Files.createTempDirectory("l5w57umr").toFile(), "m.jar");
        String innerName = Inner.class.getName().replace('.', '/') + ".class";
        byte[] innerBytes;
        try (InputStream in = L5W57UserModuleResources.class.getClassLoader().getResourceAsStream(innerName)) {
            innerBytes = in.readAllBytes();
        }
        try (JarOutputStream out = new JarOutputStream(new FileOutputStream(jar))) {
            out.putNextEntry(new ZipEntry("a.txt"));
            out.write("hello".getBytes(StandardCharsets.US_ASCII));
            out.closeEntry();
            out.putNextEntry(new ZipEntry(innerName));
            out.write(innerBytes);
            out.closeEntry();
        }
        URLClassLoader ucl = new URLClassLoader(new URL[] {jar.toURI().toURL()}, null);
        Module m = Class.forName(Inner.class.getName(), false, ucl).getModule();
        String self = "L5W57UserModuleResources.class";
        for (String tag : new String[] {"1", "2"}) {
            System.out.println("own-" + tag + "=" + attempt(() -> text(m.getResourceAsStream("a.txt"))));
            System.out.println("app-" + tag + "=" + attempt(() -> head(m.getResourceAsStream(self))));
            System.out.println("named-" + tag + "=" + attempt(() ->
                    head(Object.class.getModule().getResourceAsStream("java/lang/Object.class"))));
        }
    }
}
