// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Does `FileChannel.open` survive a young collection landing inside the
// native that builds the channel?
//
// `FileSystemProvider.newFileChannel` is registered as a Rust closure that
// allocates a `java.io.FileDescriptor`, then allocates a String for the path,
// then initializes `sun/nio/ch/FileChannelImpl`, and only THEN passes the
// descriptor into `FileChannelImpl.open`. Between the allocation and the use,
// the only reference to that descriptor is a Rust local. Under the
// Generational collector's non-moving young sweep an object nothing else roots
// is ZEROED IN PLACE, and a zeroed `FileDescriptor` reads its `fd`/`handle`
// back as 0 — which is how a real channel ends up reporting an invalid fd.
//
// This fixture is the local stand-in for
// `known-issues/springboot/generational-non-moving-sweep-zeroes-a-live-filechannel-20260906.md`,
// whose reproducer is an embedded Kafka broker on Azure. It needs only a temp
// directory and a few seconds.
//
// THE SHAPE THAT MATTERS: open a channel while ANOTHER THREAD allocates hard.
// A single-threaded fixture only collects when it allocates past the
// threshold, so the collection can only land where that thread happens to be.
// A background allocator lets a young collection land at ANY safepoint,
// including the ones inside the native — which is why the original report came
// from a multi-threaded Kafka broker and not from a loop.
//
// Usage: java NioChannelChurn <opens> <churnPerOpen> [allocatorThreads]
import java.io.IOException;
import java.nio.ByteBuffer;
import java.nio.channels.FileChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import java.util.ArrayList;

public class NioChannelChurn {

    // Kept alive across rounds so the collector has survivors to relocate,
    // and churned so young does not simply fill monotonically.
    static ArrayList<Object> retained = new ArrayList<>();

    static void churn(int n) {
        for (int i = 0; i < n; i++) {
            byte[] junk = new byte[4096];
            junk[i % junk.length] = (byte) i;
            if (junk[0] == 77 && junk[1] == 88) System.out.println("unreachable");
        }
        retained.add(new long[512]);
        if (retained.size() > 32) {
            retained.subList(0, 16).clear();
        }
    }

    static volatile boolean running = true;

    /// Background allocation, so a young collection can be requested while the
    /// main thread is inside `FileSystemProvider.newFileChannel`.
    static Thread allocator(int perBatch) {
        Thread t = new Thread(() -> {
            ArrayList<Object> local = new ArrayList<>();
            int i = 0;
            while (running) {
                for (int k = 0; k < perBatch; k++) {
                    local.add(new byte[2048]);
                }
                if (local.size() > 512) local.subList(0, 256).clear();
                i++;
                if (i == Integer.MIN_VALUE) System.out.println("unreachable");
            }
        }, "allocator");
        t.setDaemon(true);
        return t;
    }

    public static void main(String[] args) throws Exception {
        int opens = args.length > 0 ? Integer.parseInt(args[0]) : 400;
        int churnPer = args.length > 1 ? Integer.parseInt(args[1]) : 200;
        int threads = args.length > 2 ? Integer.parseInt(args[2]) : 3;

        Path dir = Files.createTempDirectory("niochurn");
        Path file = dir.resolve("data.bin");
        byte[] seed = new byte[64 * 1024];
        for (int i = 0; i < seed.length; i++) seed[i] = (byte) i;
        Files.write(file, seed);

        ArrayList<Thread> allocs = new ArrayList<>();
        for (int i = 0; i < threads; i++) { Thread t = allocator(64); allocs.add(t); t.start(); }

        long okCount = 0, bytesSeen = 0, bad = 0;
        for (int r = 0; r < opens; r++) {
            churn(churnPer);
            try (FileChannel ch = FileChannel.open(file, StandardOpenOption.READ)) {
                long size = ch.size();
                if (size != seed.length) {
                    bad++;
                    System.out.println("BAD size=" + size + " expected=" + seed.length
                            + " at open " + r);
                    continue;
                }
                ByteBuffer buf = ByteBuffer.allocate(1024);
                int got = ch.read(buf);
                if (got != 1024) {
                    bad++;
                    System.out.println("BAD read=" + got + " at open " + r);
                    continue;
                }
                bytesSeen += got;
                okCount++;
            } catch (IOException e) {
                // The page's symptom arrives as an IOException naming the fd.
                bad++;
                System.out.println("BAD IOException at open " + r + ": " + e.getMessage());
            }
            churn(churnPer);
        }

        running = false;
        for (Thread t : allocs) t.join(2000);
        Files.deleteIfExists(file);
        Files.deleteIfExists(dir);
        System.out.println("opens=" + opens + " ok=" + okCount + " bad=" + bad
                + " bytes=" + bytesSeen);
        System.out.println("NIO_CHANNEL_CHURN_DONE");
    }
}
