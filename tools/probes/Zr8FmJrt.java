// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zr z8/fm (2026-10-13): the jrt:/ image buffer and the run-time image read
// through a direct mapped buffer, for
// docs/known-issues/footprint/boot-footprint-owners-20261013.md.
//
// Usage:
//   Zr8FmJrt jrt [holdMs]   mount jrt:/, walk /modules/java.base, read every
//                           .class file's bytes, and print the count and one
//                           CRC32 over all of them (in sorted path order), then
//                           the `java.lang.Object` class file's length read
//                           back through the jrt Path API. The run's stdout is
//                           identical on HotSpot and CratonVM, whatever buffer
//                           the image reader was handed.
//   Zr8FmJrt map            map $java.home/lib/modules READ_ONLY through
//                           FileChannel.map and read its header twice: through
//                           the mapped buffer's typed views (absolute getInt,
//                           asIntBuffer on a slice, a bulk get) and through a
//                           plain FileChannel.read into a heap buffer. Prints
//                           `same` when both agree. This is the read pattern
//                           BasicImageReader applies to the buffer
//                           NativeImageBuffer.getNativeMap returns.
// stdout:
//   zr8fm jrt classes=<n> crc=<hex> object_len=<n> done
//   zr8fm map magic=<hex> version=<maj>.<min> index_ints=<n> same=<bool> done
// stderr (Linux only, -1 elsewhere): zr8fm-hold rss_kb= hwm_kb= used=
import java.io.BufferedReader;
import java.io.FileReader;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.IntBuffer;
import java.nio.MappedByteBuffer;
import java.nio.channels.FileChannel;
import java.nio.file.FileSystem;
import java.nio.file.FileSystems;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.zip.CRC32;

public class Zr8FmJrt {
    static long field(String line) {
        String[] parts = line.trim().split("\\s+");
        return Long.parseLong(parts[1]);
    }

    static String status() {
        long rss = -1, hwm = -1;
        try (BufferedReader r = new BufferedReader(new FileReader("/proc/self/status"))) {
            String line;
            while ((line = r.readLine()) != null) {
                if (line.startsWith("VmRSS:")) {
                    rss = field(line);
                } else if (line.startsWith("VmHWM:")) {
                    hwm = field(line);
                }
            }
        } catch (Exception e) {
            // not Linux
        }
        Runtime rt = Runtime.getRuntime();
        return "rss_kb=" + rss + " hwm_kb=" + hwm + " used=" + (rt.totalMemory() - rt.freeMemory());
    }

    static void jrt(long holdMs) throws Exception {
        FileSystem jrt = FileSystems.getFileSystem(java.net.URI.create("jrt:/"));
        Path root = jrt.getPath("/modules/java.base");
        List<String> names = new ArrayList<>();
        try (java.util.stream.Stream<Path> s = Files.walk(root)) {
            s.forEach(p -> {
                String f = root.relativize(p).toString();
                if (f.endsWith(".class")) {
                    names.add(f);
                }
            });
        }
        Collections.sort(names);
        CRC32 crc = new CRC32();
        for (String n : names) {
            crc.update(Files.readAllBytes(root.resolve(n)));
        }
        long objectLen = Files.size(jrt.getPath("/modules/java.base/java/lang/Object.class"));
        System.out.println("zr8fm jrt classes=" + names.size() + " crc="
                + Long.toHexString(crc.getValue()) + " object_len=" + objectLen + " done");
        System.err.println("zr8fm-hold " + status());
        if (holdMs > 0) {
            Thread.sleep(holdMs);
        }
    }

    static void map() throws Exception {
        Path image = Path.of(System.getProperty("java.home"), "lib", "modules");
        try (FileChannel ch = FileChannel.open(image, StandardOpenOption.READ)) {
            MappedByteBuffer mapped = ch.map(FileChannel.MapMode.READ_ONLY, 0, ch.size());
            ByteBuffer heap = ByteBuffer.allocate(4096);
            while (heap.hasRemaining() && ch.read(heap, heap.position()) > 0) {
                // fill the first page through a plain read
            }
            heap.flip();
            ByteOrder order = ByteOrder.nativeOrder();
            ByteBuffer ro = mapped.asReadOnlyBuffer();
            ro.order(order);
            heap.order(order);
            boolean same = true;
            // absolute typed reads
            for (int i = 0; i + 4 <= 1024; i += 4) {
                same &= ro.getInt(i) == heap.getInt(i);
            }
            // a sliced IntBuffer view, as BasicImageReader.intBuffer builds
            IntBuffer view = ro.duplicate().position(28).limit(28 + 1024).slice().order(order)
                    .asIntBuffer();
            int n = view.remaining();
            for (int i = 0; i < n; i++) {
                same &= view.get(i) == heap.getInt(28 + i * 4);
            }
            // a bulk get
            byte[] bulk = new byte[2048];
            ro.duplicate().position(100).get(bulk);
            for (int i = 0; i < bulk.length; i++) {
                same &= bulk[i] == heap.get(100 + i);
            }
            int magic = ro.getInt(0);
            int version = ro.getInt(4);
            System.out.println("zr8fm map magic=" + Integer.toHexString(magic) + " version="
                    + (version >>> 16) + "." + (version & 0xffff) + " index_ints=" + n
                    + " same=" + same + " done");
        }
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "jrt";
        if (mode.equals("map")) {
            map();
            return;
        }
        long holdMs = args.length > 1 ? Long.parseLong(args[1]) : 0L;
        jrt(holdMs);
    }
}
