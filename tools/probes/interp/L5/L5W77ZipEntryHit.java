// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3, wave 77, lane L5 -- what `ZipFile.getEntry`,
// `JarFile.getJarEntry` and `ZipFile.getInputStream` answer around an entry
// HIT (`docs/internal/fixed-bugs/interpreter-i76-L5-proposal-zip-entry-hit-in-one-native-20261109-DONE-20261110.md`).
//
// CRATONVM_ZIP_ENTRY_HIT_INTRINSIC=1 answers `ZipFile$Source.getEntryPos`'s
// exact `UTF8ZipCoder` match natively (the `EntryPos` record built with the
// caller's own name) and continues a hash collision natively; a directory
// match, a non-ASCII name and a ZipFile opened with another charset run the
// JDK body. The output must be the block below with the switch on and off
// (and with CRATONVM_ZIP_ENTRY_MISS_INTRINSIC=1 beside it), in both modes,
// with and without the JIT. `L5W76ZipEntryMiss` is the miss-side twin and
// must hold too.
//
// The cases: two stored names sharing a String.hashCode (each found: the one
// behind the other on the chain by continuing past a NO_MATCH), a name and
// its directory form both stored in either order (`getEntry("a")` is "a"
// whether the chain meets "a/" first -- a DIRECTORY_MATCH, then the exact
// one -- or "a" first), a directory alone (`getEntry("b")` is "b/";
// `getInputStream` of "b" is null: no addSlash), the entry name's identity
// (an exact hit hands back the caller's own String; a directory hit a new
// one), a ZipFile opened with ISO-8859-1 (its coder is not the UTF-8 one;
// the entries carry the UTF-8 flag, so the non-ASCII name is still found),
// a JarFile, and 2000 repeated hits (the native path after the first,
// which runs the JDK body and initialises `ZipFile$EntryPos`).
//
// Run:
//   $J/javac -d out L5W77ZipEntryHit.java
//   $J/java -cp out L5W77ZipEntryHit                                                   # HotSpot
//   $B --java-home $J/.. -cp out L5W77ZipEntryHit                                      # off
//   env CRATONVM_ZIP_ENTRY_HIT_INTRINSIC=1 $B --java-home $J/.. -cp out L5W77ZipEntryHit
// (each CratonVM line also with --nojit and with --compatible).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// collide get q0/AaX.class -> q0/AaX.class
// collide get q0/BBX.class -> q0/BBX.class
// collide get q0/P1.class -> q0/P1.class
// collide get q0/AaY.class -> null
// collide stream q0/BBX.class -> bytes=6
// dir-first get a -> a
// dir-first get a/ -> a/
// file-first get a -> a
// file-first get a/ -> a/
// dir-only get b -> b/
// dir-only get b/ -> b/
// dir-only stream b -> null
// identity exact=true dir=false
// latin1 get q0/P1.class -> q0/P1.class
// latin1 get q0/BBX.class -> q0/BBX.class
// latin1 get <e9>t<e9>.txt -> <e9>t<e9>.txt
// latin1 get q0/P2.class -> null
// utf8 get <e9>t<e9>.txt -> <e9>t<e9>.txt
// jar get q0/AaX.class -> q0/AaX.class
// jar get q0/BBX.class -> q0/BBX.class
// jar get META-INF/MANIFEST.MF -> null
// repeat hits=2000 same-name=2000 size-sum=10000
// HOTSPOT_EXPECTED_END

import java.io.File;
import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.jar.JarEntry;
import java.util.jar.JarFile;
import java.util.zip.ZipEntry;
import java.util.zip.ZipFile;
import java.util.zip.ZipOutputStream;

public class L5W77ZipEntryHit {
    static final String ETE = "été.txt";

    static String show(String s) {
        if (s == null) {
            return "null";
        }
        StringBuilder b = new StringBuilder();
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) {
                b.append('<').append(Integer.toHexString(c)).append('>');
            } else {
                b.append(c);
            }
        }
        return b.toString();
    }

    static File write(File dir, String file, String[] names) throws IOException {
        File f = new File(dir, file);
        try (ZipOutputStream out = new ZipOutputStream(Files.newOutputStream(f.toPath()))) {
            for (String n : names) {
                out.putNextEntry(new ZipEntry(n));
                if (!n.endsWith("/")) {
                    out.write(n.length() > 5 ? "hello!".getBytes(StandardCharsets.US_ASCII)
                                             : "hello".getBytes(StandardCharsets.US_ASCII));
                }
                out.closeEntry();
            }
        }
        return f;
    }

    static void get(String label, ZipFile zip, String name) {
        ZipEntry e = zip.getEntry(name);
        System.out.println(label + " get " + show(name) + " -> " + (e == null ? "null" : show(e.getName())));
    }

    static void stream(String label, ZipFile zip, String name) throws IOException {
        try (InputStream in = zip.getInputStream(new ZipEntry(name))) {
            System.out.println(label + " stream " + show(name) + " -> "
                    + (in == null ? "null" : "bytes=" + in.readAllBytes().length));
        }
    }

    public static void main(String[] args) throws Exception {
        File dir = Files.createTempDirectory("l5w77hit").toFile();
        try {
            File collide = write(dir, "collide.zip",
                    new String[] {"q0/", "q0/AaX.class", "q0/P1.class", "q0/BBX.class", ETE});
            try (ZipFile zip = new ZipFile(collide)) {
                get("collide", zip, "q0/AaX.class");
                get("collide", zip, "q0/BBX.class");
                get("collide", zip, "q0/P1.class");
                get("collide", zip, "q0/AaY.class");
                stream("collide", zip, "q0/BBX.class");
            }
            try (ZipFile zip = new ZipFile(write(dir, "dirfirst.zip", new String[] {"a/", "a"}))) {
                get("dir-first", zip, "a");
                get("dir-first", zip, "a/");
            }
            try (ZipFile zip = new ZipFile(write(dir, "filefirst.zip", new String[] {"a", "a/"}))) {
                get("file-first", zip, "a");
                get("file-first", zip, "a/");
            }
            try (ZipFile zip = new ZipFile(write(dir, "dironly.zip", new String[] {"b/", "c.txt"}))) {
                get("dir-only", zip, "b");
                get("dir-only", zip, "b/");
                stream("dir-only", zip, "b");
                String exact = new String("c.txt".toCharArray());
                String asDir = new String("b".toCharArray());
                boolean sameExact = zip.getEntry(exact).getName() == exact;
                boolean sameDir = zip.getEntry(asDir).getName() == asDir;
                System.out.println("identity exact=" + sameExact + " dir=" + sameDir);
            }
            try (ZipFile zip = new ZipFile(collide, StandardCharsets.ISO_8859_1)) {
                get("latin1", zip, "q0/P1.class");
                get("latin1", zip, "q0/BBX.class");
                get("latin1", zip, ETE);
                get("latin1", zip, "q0/P2.class");
            }
            try (ZipFile zip = new ZipFile(collide)) {
                get("utf8", zip, ETE);
            }
            try (JarFile jar = new JarFile(collide)) {
                for (String n : new String[] {"q0/AaX.class", "q0/BBX.class", "META-INF/MANIFEST.MF"}) {
                    JarEntry e = jar.getJarEntry(n);
                    System.out.println("jar get " + n + " -> " + (e == null ? "null" : e.getName()));
                }
            }
            try (ZipFile zip = new ZipFile(collide)) {
                int hits = 0;
                int same = 0;
                long sizes = 0;
                String[] names = {"q0/AaX.class", "q0/BBX.class", "q0/P1.class", "q0/AaX.class"};
                for (int i = 0; i < 2000; i++) {
                    String n = names[i & 3];
                    ZipEntry e = zip.getEntry(n);
                    if (e != null) {
                        hits++;
                        if (e.getName() == n) {
                            same++;
                        }
                        sizes += e.getSize() > 5 ? 5 : e.getSize();
                    }
                }
                System.out.println("repeat hits=" + hits + " same-name=" + same + " size-sum=" + sizes);
            }
        } finally {
            File[] files = dir.listFiles();
            if (files != null) {
                for (File f : files) {
                    f.delete();
                }
            }
            dir.delete();
        }
    }
}
