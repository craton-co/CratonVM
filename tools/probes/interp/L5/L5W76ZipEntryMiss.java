// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3, wave 76, lane L5 -- what `ZipFile.getEntry`,
// `JarFile.getJarEntry` and `ZipFile.getInputStream` answer around an entry
// miss (`docs/internal/fixed-bugs/interpreter-i75-L5-proposal-zip-entry-miss-without-the-java-walk-20261108-KEPT-OFF-20261109.md`).
//
// CRATONVM_ZIP_ENTRY_MISS_INTRINSIC=1 answers `ZipFile$Source.getEntryPos`'s
// no-hash-match miss natively and runs the JDK body for every hash hit; the
// output must be the block below with the switch on and off, in both modes.
// The cases: exact hits, the directory form (`q0` finds `q0/`), a
// String.hashCode collision (`q0/BBX.class` against the stored
// `q0/AaX.class`: a hash hit whose compare fails), plain misses, the empty
// name, non-ASCII names, `getInputStream` of a foreign entry (addSlash
// false: `dir` must NOT find `dir/`), an empty archive (total == 0), a null
// name and a closed file.
//
// Run:
//   $J/javac -d out L5W76ZipEntryMiss.java
//   $J/java -cp out L5W76ZipEntryMiss                                                   # HotSpot
//   $B --java-home $J/.. -cp out L5W76ZipEntryMiss                                      # off
//   env CRATONVM_ZIP_ENTRY_MISS_INTRINSIC=1 $B --java-home $J/.. -cp out L5W76ZipEntryMiss
// (each CratonVM line also with --nojit and with --compatible).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// zip get q0/P1.class -> q0/P1.class
// zip get q0/AaX.class -> q0/AaX.class
// zip get q0 -> q0/
// zip get q0/ -> q0/
// zip get dir -> dir/
// zip get dir/x.txt -> dir/x.txt
// zip get été.txt -> été.txt
// zip get q0/BBX.class -> null
// zip get q0/P2.class -> null
// zip get q1/P1.class -> null
// zip get dir/y.txt -> null
// zip get  -> null
// zip get / -> null
// zip get été -> null
// zip get java/lang/Object.class -> null
// zip generated misses=1000 hits=0
// jar get q0/P1.class -> q0/P1.class
// jar get q0/BBX.class -> null
// jar get q0/P2.class -> null
// jar get dir -> dir/
// stream q0/P1.class -> bytes=5
// stream q0/P2.class -> null
// stream dir -> null
// stream dir/ -> bytes=0
// stream q0/BBX.class -> null
// empty size=0 get a -> null
// empty get null -> NullPointerException: name
// zip get null -> NullPointerException: name
// closed get q0/P1.class -> IllegalStateException: zip file closed
// HOTSPOT_EXPECTED_END

import java.io.File;
import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.jar.JarFile;
import java.util.zip.ZipEntry;
import java.util.zip.ZipFile;
import java.util.zip.ZipOutputStream;

public class L5W76ZipEntryMiss {
    static final String[] STORED = {
        "q0/", "q0/P1.class", "q0/AaX.class", "dir/", "dir/x.txt", "été.txt",
    };

    static String show(String s) {
        StringBuilder b = new StringBuilder();
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) {
                b.append(String.format("\\u%04x", (int) c));
            } else {
                b.append(c);
            }
        }
        return b.toString();
    }

    static void write(File file, String[] names) throws IOException {
        try (ZipOutputStream zip = new ZipOutputStream(Files.newOutputStream(file.toPath()))) {
            for (String name : names) {
                ZipEntry e = new ZipEntry(name);
                e.setTime(1577836800000L);
                zip.putNextEntry(e);
                if (!name.endsWith("/")) {
                    zip.write("hello".getBytes(StandardCharsets.US_ASCII));
                }
                zip.closeEntry();
            }
        }
    }

    static String entryName(ZipEntry e) {
        return e == null ? "null" : show(e.getName());
    }

    static String stream(ZipFile zip, String name) throws IOException {
        try (InputStream in = zip.getInputStream(new ZipEntry(name))) {
            return in == null ? "null" : "bytes=" + in.readAllBytes().length;
        }
    }

    static String thrown(Runnable r) {
        try {
            r.run();
            return "no exception";
        } catch (RuntimeException e) {
            return e.getClass().getSimpleName() + ": " + e.getMessage();
        }
    }

    public static void main(String[] args) throws Exception {
        File dir = Files.createTempDirectory("l5w76miss").toFile();
        File full = new File(dir, "full.jar");
        File empty = new File(dir, "empty.zip");
        try {
            write(full, STORED);
            write(empty, new String[0]);

            String[] asks = {
                "q0/P1.class", "q0/AaX.class", "q0", "q0/", "dir", "dir/x.txt", "été.txt",
                "q0/BBX.class", "q0/P2.class", "q1/P1.class", "dir/y.txt", "", "/", "été",
                "java/lang/Object.class",
            };
            try (ZipFile zip = new ZipFile(full)) {
                for (String name : asks) {
                    System.out.println("zip get " + show(name) + " -> " + entryName(zip.getEntry(name)));
                }
                int misses = 0;
                int hits = 0;
                for (int i = 0; i < 1000; i++) {
                    if (zip.getEntry("x" + (i % 7) + "/N" + i + ".class") == null) {
                        misses++;
                    } else {
                        hits++;
                    }
                }
                System.out.println("zip generated misses=" + misses + " hits=" + hits);
            }
            try (JarFile jar = new JarFile(full)) {
                for (String name : new String[] {"q0/P1.class", "q0/BBX.class", "q0/P2.class", "dir"}) {
                    System.out.println("jar get " + name + " -> " + entryName(jar.getJarEntry(name)));
                }
            }
            // A fresh ZipFile: no getEntry before, so `lastEntryName` is null
            // and every getInputStream runs the lookup (addSlash false).
            try (ZipFile zip = new ZipFile(full)) {
                for (String name : new String[] {"q0/P1.class", "q0/P2.class", "dir", "dir/", "q0/BBX.class"}) {
                    System.out.println("stream " + name + " -> " + stream(zip, name));
                }
            }
            try (ZipFile zip = new ZipFile(empty)) {
                System.out.println("empty size=" + zip.size() + " get a -> " + entryName(zip.getEntry("a")));
                System.out.println("empty get null -> " + thrown(() -> zip.getEntry(null)));
            }
            ZipFile zip = new ZipFile(full);
            System.out.println("zip get null -> " + thrown(() -> zip.getEntry(null)));
            zip.close();
            System.out.println("closed get q0/P1.class -> " + thrown(() -> zip.getEntry("q0/P1.class")));
        } finally {
            full.delete();
            empty.delete();
            dir.delete();
        }
    }
}
