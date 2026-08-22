// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.BufferPoolMXBean;
import java.lang.management.ManagementFactory;
import java.nio.ByteBuffer;
import java.nio.channels.FileChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import java.util.List;

/**
 * Is `sun.nio.ch.Util`'s per-thread temporary-direct-buffer cache working?
 *
 * A `FileChannel.read` into a HEAP buffer cannot DMA into the Java array, so
 * the JDK borrows a direct buffer from `Util.getTemporaryDirectBuffer`, reads
 * into it, and copies out. That helper keeps a small per-thread cache, so a
 * loop doing N such reads should allocate ONE direct buffer, not N.
 *
 * `BufferPoolMXBean("direct")` counts exactly that, from plain Java. If the
 * cache works, `count` is flat across the loop. If it does not, `count`
 * climbs with the loop and every read also registers a Cleaner — which is
 * what a stack sample of CratonVM loading a GGUF model kept catching
 * (`DirectByteBuffer.<init>` and `CleanerImpl.run` between them took 12 of
 * 16 samples).
 *
 * This is deliberately observable through a public MXBean rather than by
 * reaching into `sun.nio.ch` internals, so the same probe runs unmodified on
 * HotSpot as the reference.
 */
public class DirectBufferCacheProbe {

    static BufferPoolMXBean directPool() {
        List<BufferPoolMXBean> pools =
                ManagementFactory.getPlatformMXBeans(BufferPoolMXBean.class);
        for (BufferPoolMXBean p : pools) {
            if ("direct".equals(p.getName())) {
                return p;
            }
        }
        throw new IllegalStateException("no 'direct' BufferPoolMXBean");
    }

    public static void main(String[] args) throws Exception {
        int reads = args.length > 0 ? Integer.parseInt(args[0]) : 5000;
        BufferPoolMXBean direct = directPool();

        Path tmp = Files.createTempFile("dbufprobe", ".bin");
        try {
            byte[] filler = new byte[1 << 20];
            try (FileChannel out = FileChannel.open(tmp, StandardOpenOption.WRITE)) {
                for (int i = 0; i < 8; i++) {
                    out.write(ByteBuffer.wrap(filler));
                }
            }

            long countBefore = direct.getCount();
            long usedBefore = direct.getMemoryUsed();

            long checksum = 0;
            long t0 = System.nanoTime();
            try (FileChannel ch = FileChannel.open(tmp, StandardOpenOption.READ)) {
                for (int i = 0; i < reads; i++) {
                    byte[] body = new byte[24];
                    ch.read(ByteBuffer.wrap(body));
                    checksum += body[0];
                    if (ch.position() > (1 << 20) * 7L) {
                        ch.position(0);
                    }
                }
            }
            long dt = System.nanoTime() - t0;

            long countAfter = direct.getCount();
            long usedAfter = direct.getMemoryUsed();

            // The headline: direct buffers allocated PER READ. A working
            // cache makes this ~0; a broken one makes it ~1.
            double perRead = (countAfter - countBefore) / (double) reads;
            System.out.println("DBUFCACHE reads=" + reads
                    + " direct_count_before=" + countBefore
                    + " direct_count_after=" + countAfter
                    + " direct_delta=" + (countAfter - countBefore)
                    + " per_read=" + perRead
                    + " used_delta_bytes=" + (usedAfter - usedBefore)
                    + " ns_per_read=" + (dt / (double) reads)
                    + " checksum=" + checksum);
            System.out.println(perRead < 0.01
                    ? "DBUFCACHE verdict=CACHE_WORKING"
                    : "DBUFCACHE verdict=CACHE_MISSING_EVERY_READ_ALLOCATES");
        } finally {
            Files.deleteIfExists(tmp);
        }
    }
}
