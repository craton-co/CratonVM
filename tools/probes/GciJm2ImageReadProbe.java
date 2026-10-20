// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.InputStream;
import java.lang.module.ModuleFinder;
import java.lang.module.ModuleReader;
import java.lang.module.ModuleReference;
import java.net.URI;
import java.nio.ByteBuffer;
import java.nio.file.FileSystem;
import java.nio.file.FileSystems;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Optional;
import java.util.stream.Stream;
import java.util.zip.CRC32;

/**
 * gci i2/jm (2026-10-12): reads of the run-time image through every door that
 * reaches {@code jdk.internal.jimage.BasicImageReader}, whose whole-image map
 * {@code NativeImageBuffer.getNativeMap} supplies.
 *
 * <p>The map used to be the image copied into a 145 MiB Java {@code byte[]}
 * ({@code docs/known-issues/gc/gci-i1orch-jimage-map-is-a-145-mib-java-heap-array-20261012.md});
 * it is now a direct buffer over a read-only memory map, as on HotSpot. Every
 * read the reader makes of it goes through a raw address: the header and the
 * redirect / offsets tables ({@code slice().asIntBuffer().get(int)}), the
 * location attributes and strings ({@code get(int)}), and resource bytes
 * (a {@code slice} handed out by {@code ModuleReader.read}, or bulk-copied
 * into a {@code byte[]} by {@code getResource}). A wrong read changes a count
 * or a CRC below.
 *
 * <p>stdout (deterministic; HotSpot 25 is the oracle, with the SAME JDK image):
 * one line per round, then {@code PASS image-read rounds=R}. Run it at a heap
 * HotSpot needs and the old heap copy cannot fit in (64m): the heap-copy arm
 * ({@code CRATONVM_JIMAGE_DIRECT_MAP=0}) is expected to fail there.
 *
 * <p>Run: {@code cratonvm -Xmx64m -cp <classes> GciJm2ImageReadProbe 3}.
 */
public class GciJm2ImageReadProbe {
    static long crc(byte[] b) {
        CRC32 c = new CRC32();
        c.update(b, 0, b.length);
        return c.getValue();
    }

    static String hex(long v) {
        return Long.toHexString(v);
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 3;
        long t0 = System.nanoTime();
        for (int r = 1; r <= rounds; r++) {
            StringBuilder line = new StringBuilder("round ").append(r);

            // 1. java.base's ModuleReader: list() walks the image directory
            //    (strings, locations), read() hands out a slice of the map.
            ModuleReference base = ModuleFinder.ofSystem().find("java.base").orElseThrow();
            try (ModuleReader reader = base.open()) {
                long langClasses;
                try (Stream<String> names = reader.list()) {
                    langClasses = names
                            .filter(n -> n.startsWith("java/lang/") && n.endsWith(".class"))
                            .count();
                }
                line.append(" langClasses=").append(langClasses);
                Optional<ByteBuffer> object = reader.read("java/lang/Object.class");
                if (object.isPresent()) {
                    ByteBuffer bb = object.get();
                    boolean direct = bb.isDirect();
                    byte[] bytes = new byte[bb.remaining()];
                    bb.get(bytes);
                    reader.release(bb);
                    line.append(" object=").append(bytes.length).append('/').append(hex(crc(bytes)))
                            .append(" direct=").append(direct);
                } else {
                    line.append(" object=absent");
                }
                line.append(" missing=").append(reader.read("java/lang/NoSuchClass42.class").isPresent());
            }

            // 2. The jrt: file system (jrtfs over the same ImageReader).
            FileSystem jrt = FileSystems.getFileSystem(URI.create("jrt:/"));
            byte[] string = Files.readAllBytes(jrt.getPath("/modules/java.base/java/lang/String.class"));
            line.append(" string=").append(string.length).append('/').append(hex(crc(string)));
            long sqlClasses;
            try (Stream<Path> walk = Files.walk(jrt.getPath("/modules/java.sql"))) {
                sqlClasses = walk.filter(p -> p.toString().endsWith(".class")).count();
            }
            line.append(" sqlClasses=").append(sqlClasses);

            // 3. A platform-module class's own resource.
            try (InputStream in = Class.forName("java.sql.Connection").getResourceAsStream("Connection.class")) {
                if (in == null) {
                    line.append(" connection=null");
                } else {
                    byte[] c = in.readAllBytes();
                    line.append(" connection=").append(c.length).append('/').append(hex(crc(c)));
                }
            }

            System.out.println(line);
        }
        System.out.println("PASS image-read rounds=" + rounds);
        System.err.println("[image-read] ms=" + (System.nanoTime() - t0) / 1_000_000);
    }
}
