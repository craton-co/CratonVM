// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 57, lane L5 -- `Path.toFile()` is
// `new File(toString())`, so its `File` is normalized like any other, and a
// `URLClassLoader` over a jar in `Files.createTempDirectory(..).toFile()`
// finds the jar's entries.
//
// Rows (the first call, and again last):
//   tofile-*  `Paths.get(<tmp>, "a//b/").toFile()`: separators of `getPath()`
//             (`<sep>` for File.separator), `isAbsolute()`, equal to the same
//             `new File(...)`, equal hash codes
//   child-*   `new File(<tmp path>.toFile(), "x.jar")`: absolute?
//   ucl-*     a `URLClassLoader` over `<createTempDirectory>.toFile()/res.jar`
//             finds `a.txt`
//
// Run (no setup):
//   javac -d out L5W57CompatPathToFile.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W57CompatPathToFile
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   tofile-1=<tmp><sep>a<sep>b abs=true eq=true hash=true
//   child-1=abs=true
//   ucl-1=found=true
//   tofile-2=<tmp><sep>a<sep>b abs=true eq=true hash=true
//   child-2=abs=true
//   ucl-2=found=true
//
// CratonVM before wave 57, debug build, Windows, `--compatible` (measured):
// `tofile-N=<tmp>/a/b abs=true eq=false hash=false` (the slashes kept),
// `child-N=abs=false`, `ucl-N=found=false`; `--jdk-only` matched.

import java.io.File;
import java.io.FileOutputStream;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.jar.JarOutputStream;
import java.util.zip.ZipEntry;

public class L5W57CompatPathToFile {
    static String attempt(java.util.concurrent.Callable<Object> c) {
        try {
            return String.valueOf(c.call());
        } catch (Throwable t) {
            return t.getClass().getName() + " msg=" + t.getMessage();
        }
    }

    static void rows(String tag, Path tmp) {
        String tmpText = new File(tmp.toString()).getPath();
        System.out.println("tofile-" + tag + "=" + attempt(() -> {
            File f = Paths.get(tmp.toString(), "a//b/").toFile();
            File g = new File(tmpText + File.separator + "a" + File.separator + "b");
            String shown = f.getPath();
            if (shown.startsWith(tmpText)) {
                shown = "<tmp>" + shown.substring(tmpText.length());
            }
            shown = shown.replace(File.separator, "<sep>");
            return shown + " abs=" + f.isAbsolute() + " eq=" + f.equals(g) + " hash="
                    + (f.hashCode() == g.hashCode());
        }));
        System.out.println("child-" + tag + "=" + attempt(() -> "abs=" + new File(tmp.toFile(), "x.jar").isAbsolute()));
        System.out.println("ucl-" + tag + "=" + attempt(() -> {
            File jar = new File(tmp.toFile(), "res" + tag + ".jar");
            try (JarOutputStream out = new JarOutputStream(new FileOutputStream(jar))) {
                out.putNextEntry(new ZipEntry("a.txt"));
                out.write("hello".getBytes(StandardCharsets.US_ASCII));
                out.closeEntry();
            }
            try (URLClassLoader ucl = new URLClassLoader(new URL[] {jar.toURI().toURL()}, null)) {
                return "found=" + (ucl.getResource("a.txt") != null);
            }
        }));
    }

    public static void main(String[] args) throws Exception {
        Path tmp = Files.createTempDirectory("l5w57ptf");
        rows("1", tmp);
        rows("2", tmp);
    }
}
