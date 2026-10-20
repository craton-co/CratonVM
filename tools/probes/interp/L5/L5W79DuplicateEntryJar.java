// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3, wave 79, lane L5 -- a class-path jar with two central
// directory records of one name
// (`docs/internal/fixed-bugs/interpreter-i71-L5-the-plain-view-duplicate-name-check-is-always-true-FIXED-20261111.md`).
//
// `java.util.zip.ZipFile` keeps every CEN record (`size()` counts both) and
// resolves a name through its CEN hash table, whose chain starts at the LAST
// record added (`ZipFile$Source.initCEN` / `getEntryPos`), so a lookup reads
// the second record. CratonVM's jar-loader rows meant to refuse such a jar
// (no "plain view": the JDK body answers) but their duplicate check could
// never fire; since wave 79 it counts the records. The answer below must be
// the same with the rows on or off: this probe guards that the rows never
// serve another record than the JDK body does.
//
// The jar is written with `p/A.txt` and `p/B.txt` and then `p/B.txt` is
// renamed `p/A.txt` in place (same length) in its local header and its CEN
// record (`ZipOutputStream` refuses a duplicate name).
//
// Run:
//   $J/javac -d out L5W79DuplicateEntryJar.java
//   $J/java -cp out L5W79DuplicateEntryJar                                    # HotSpot
//   $B --java-home $J/.. -cp out L5W79DuplicateEntryJar
//   env CRATONVM_JAR_LOADER_BYTES_INDEX=1 CRATONVM_JAR_LOADER_CLASS_PATH_DIRECT=1 $B --java-home $J/.. -cp out L5W79DuplicateEntryJar
// (each also with --compatible). The expected block is read from the JDK 25
// sources; confirm it on the host first.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// zipfile size=2 read=other
// loader read=other
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.InputStream;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.zip.ZipEntry;
import java.util.zip.ZipFile;
import java.util.zip.ZipOutputStream;

public class L5W79DuplicateEntryJar {
    static byte[] jarWithDuplicate() throws Exception {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (ZipOutputStream zip = new ZipOutputStream(bytes)) {
            for (String[] e : new String[][] {{"p/A.txt", "first"}, {"p/B.txt", "other"}}) {
                ZipEntry entry = new ZipEntry(e[0]);
                entry.setTime(1577836800000L);
                zip.putNextEntry(entry);
                zip.write(e[1].getBytes("US-ASCII"));
                zip.closeEntry();
            }
        }
        byte[] b = bytes.toByteArray();
        byte[] from = "p/B.txt".getBytes("US-ASCII");
        byte[] to = "p/A.txt".getBytes("US-ASCII");
        for (int i = 0; i + from.length <= b.length; i++) {
            boolean match = true;
            for (int k = 0; k < from.length && match; k++) {
                match = b[i + k] == from[k];
            }
            if (match) {
                System.arraycopy(to, 0, b, i, to.length);
            }
        }
        return b;
    }

    static String read(InputStream in) throws Exception {
        if (in == null) {
            return "null";
        }
        try (in) {
            return new String(in.readAllBytes(), "US-ASCII");
        }
    }

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("l5w79dup");
        Path jar = dir.resolve("dup.jar");
        try {
            Files.write(jar, jarWithDuplicate());
            try (ZipFile zf = new ZipFile(jar.toFile())) {
                System.out.println("zipfile size=" + zf.size()
                        + " read=" + read(zf.getInputStream(zf.getEntry("p/A.txt"))));
            }
            try (URLClassLoader loader = new URLClassLoader(new URL[] {jar.toUri().toURL()}, null)) {
                System.out.println("loader read=" + read(loader.getResourceAsStream("p/A.txt")));
            }
        } finally {
            new File(jar.toString()).delete();
            new File(dir.toString()).delete();
        }
    }
}
