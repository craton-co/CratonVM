// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2, wave 64, lane L5 -- timing for the jar-loader bytes
// index (`CRATONVM_JAR_LOADER_BYTES_INDEX`; see `L5W64JarLoaderBytesIndex`
// for the behaviour). A `URLClassLoader` over one plain `file:` jar of
// CLASSES classes of about 4 KB each (DEFLATED, `ZipOutputStream`'s default,
// as build tools write them) defines every class per round on a fresh loader.
// The row is the time per defined class: the JDK's `Resource.getBytes` read
// loop over the `ZipFile` inflater stream (switch off) against the VM's own
// copy of the entry (switch on). The same loop runs for every class the
// application loader defines from a `-cp` jar under `--jdk-only`.
//
// Run:
//   javac -d out L5W64JarLoaderBytesBench.java
//   cratonvm --java-home <jdk25> -cp out L5W64JarLoaderBytesBench                      # control
//   CRATONVM_JAR_LOADER_BYTES_INDEX=1 cratonvm --java-home <jdk25> -cp out L5W64JarLoaderBytesBench
//   java -cp out L5W64JarLoaderBytesBench                                               # HotSpot
// Interleave the binaries and take medians (in-VM timings swing between
// repetitions on the dev hosts).
//
// Prints one `round=<k> ns/op=<n>` row per round and `median ns/op=<n>` to
// stderr; stdout is the single line `loaded=<CLASSES> per round`, the same on
// every VM. Positive control: `CRATONVM_DBG=access` reads
// `jar-loader-bytes-served=<CLASSES * ROUNDS>` with the switch on.

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;
import java.util.zip.ZipEntry;
import java.util.zip.ZipOutputStream;

public class L5W64JarLoaderBytesBench {
    static final int CLASSES = 300;
    static final int ROUNDS = 9;

    /** `public class <name> extends Object {}` padded with an unused, compressible `CONSTANT_Utf8`. */
    static byte[] paddedClass(String name, int pad) throws IOException {
        StringBuilder text = new StringBuilder();
        for (int i = 0; i < pad; i++) {
            text.append((char) ('a' + (i * 7 + name.length()) % 26));
        }
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(49);
        out.writeShort(6);
        out.writeByte(1); out.writeUTF(name);
        out.writeByte(7); out.writeShort(1);
        out.writeByte(1); out.writeUTF("java/lang/Object");
        out.writeByte(7); out.writeShort(3);
        out.writeByte(1); out.writeUTF(text.toString());
        out.writeShort(0x21);
        out.writeShort(2);
        out.writeShort(4);
        out.writeShort(0);
        out.writeShort(0);
        out.writeShort(0);
        out.writeShort(0);
        out.flush();
        return bytes.toByteArray();
    }

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("l5w64-bench");
        Path jar = dir.resolve("bench.jar");
        try (ZipOutputStream zip = new ZipOutputStream(Files.newOutputStream(jar))) {
            zip.putNextEntry(new ZipEntry("p/"));
            zip.closeEntry();
            for (int i = 0; i < CLASSES; i++) {
                zip.putNextEntry(new ZipEntry("p/C" + i + ".class"));
                zip.write(paddedClass("p/C" + i, 4000));
                zip.closeEntry();
            }
        }
        URL[] urls = {jar.toUri().toURL()};
        long[] perClass = new long[ROUNDS];
        int loaded = 0;
        try {
            for (int r = 0; r < ROUNDS; r++) {
                loaded = 0;
                try (URLClassLoader ucl = new URLClassLoader(urls, ClassLoader.getSystemClassLoader())) {
                    long t0 = System.nanoTime();
                    for (int i = 0; i < CLASSES; i++) {
                        if (ucl.loadClass("p.C" + i).getClassLoader() == ucl) {
                            loaded++;
                        }
                    }
                    perClass[r] = (System.nanoTime() - t0) / CLASSES;
                }
                System.err.println("round=" + r + " ns/op=" + perClass[r]);
            }
        } finally {
            Files.deleteIfExists(jar);
            Files.deleteIfExists(dir);
        }
        long[] sorted = perClass.clone();
        Arrays.sort(sorted);
        System.err.println("median ns/op=" + sorted[ROUNDS / 2]);
        System.out.println("loaded=" + loaded + " per round");
    }
}
