// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.RandomAccessFile;
import java.nio.ByteBuffer;
import java.nio.channels.ClosedChannelException;
import java.nio.channels.FileChannel;
import java.nio.channels.NonReadableChannelException;
import java.nio.channels.NonWritableChannelException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;

/**
 * Regression: `sun.nio.ch.FileChannelImpl.read/write(ByteBuffer)`,
 * `position()`, `position(long)` and `size()` are answered by a Rust fast path
 * (`native-io/src/file_channel_fast_read.rs`) instead of the JDK's ~20-frame
 * glue chain, and every observable of that chain has to survive.
 *
 * The open page that asked for the fast path
 * (`performance/filechannel-heap-read-glue-depth-FIXED-20260823.md`) also gave
 * the reason it had not been taken: a fast path has to reproduce the position
 * advance, the EOF-is-`-1` convention, the read-only and non-readable
 * refusals, and the `beginBlocking`/`endBlocking` pairing — and **getting any
 * one of them wrong silently desynchronises the channel position, so every
 * subsequent read returns the wrong bytes rather than failing.** Nothing
 * throws. That is why every check below asserts an EXACT byte or position,
 * never merely "no exception".
 *
 * `RChannelInterrupt` owns the asynchronous-close half (an already-interrupted
 * thread must get `ClosedByInterruptException` and a closed channel); this
 * vector owns the data and position half, plus the shapes the fast path
 * REFUSES — a read-only destination, a direct buffer, an append descriptor, a
 * non-readable channel — because a refusal that silently answered anyway is
 * exactly as invisible as a wrong read.
 *
 * Every value is compared against a fixed constant, so HotSpot and CratonVM
 * both have to produce it; the suite additionally diffs the two runs' output.
 */
public class RFileChannelFastIo {
    static int checks = 0;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    /** 256 bytes whose value is their own index — so any misplaced read shows. */
    static byte[] ramp() {
        byte[] b = new byte[256];
        for (int i = 0; i < b.length; i++) {
            b[i] = (byte) i;
        }
        return b;
    }

    public static void main(String[] args) throws Exception {
        Path tmp = Files.createTempFile("rfcfastio", ".bin");
        try {
            try (FileChannel out = FileChannel.open(tmp, StandardOpenOption.WRITE)) {
                int n = out.write(ByteBuffer.wrap(ramp()));
                check(n == 256, "write returned " + n + ", expected 256");
                check(out.position() == 256, "position after write = " + out.position());
                check(out.size() == 256, "size after write = " + out.size());
            }

            // ---- 1. sequential reads advance the channel AND the buffer ----
            try (FileChannel ch = FileChannel.open(tmp, StandardOpenOption.READ)) {
                ByteBuffer b = ByteBuffer.allocate(8);
                int n = ch.read(b);
                check(n == 8, "first read returned " + n);
                check(b.position() == 8, "buffer position after read = " + b.position());
                check(ch.position() == 8, "channel position after read = " + ch.position());
                check(b.get(0) == 0 && b.get(7) == 7, "first 8 bytes are not 0..7");

                // A SECOND read must continue where the first stopped. A fast
                // path that forgot the position advance passes the check above
                // and fails only here.
                b.clear();
                n = ch.read(b);
                check(n == 8, "second read returned " + n);
                check(b.get(0) == 8 && b.get(7) == 15, "second 8 bytes are not 8..15");
                check(ch.position() == 16, "channel position after two reads = " + ch.position());

                // ---- 2. a partially-filled destination reads only `remaining` ----
                ByteBuffer part = ByteBuffer.allocate(16);
                part.position(10);
                n = ch.read(part);
                check(n == 6, "read into remaining=6 returned " + n);
                check(part.position() == 16, "partial buffer position = " + part.position());
                check(part.get(10) == 16, "partial read landed at the wrong offset");
                check(ch.position() == 22, "channel position after partial = " + ch.position());

                // ---- 3. an array-offset (sliced) destination ----
                byte[] backing = new byte[32];
                ByteBuffer sliced = ByteBuffer.wrap(backing, 8, 8).slice();
                n = ch.read(sliced);
                check(n == 8, "sliced read returned " + n);
                check(backing[8] == 22, "sliced read ignored the array offset");
                check(backing[7] == 0 && backing[16] == 0, "sliced read wrote out of range");

                // ---- 4. an empty destination is 0, and is NOT eof ----
                long before = ch.position();
                n = ch.read(ByteBuffer.allocate(0));
                check(n == 0, "empty-destination read returned " + n);
                check(ch.position() == before, "empty read moved the position");

                // ---- 5. absolute position, then EOF is -1 ----
                ch.position(250);
                ByteBuffer tail = ByteBuffer.allocate(16);
                n = ch.read(tail);
                check(n == 6, "read at 250 returned " + n);
                check(tail.get(0) == (byte) 250, "read at 250 got the wrong byte");
                check(ch.position() == 256, "position after tail read = " + ch.position());
                tail.clear();
                n = ch.read(tail);
                check(n == -1, "read at EOF returned " + n + ", expected -1");
                check(ch.position() == 256, "EOF read moved the position");

                // ---- 6. a read-only DESTINATION is IllegalArgumentException ----
                boolean threw = false;
                try {
                    ch.position(0);
                    ch.read(ByteBuffer.allocate(8).asReadOnlyBuffer());
                } catch (IllegalArgumentException e) {
                    threw = true;
                }
                check(threw, "read into a read-only buffer did not throw IllegalArgumentException");

                // ---- 7. a DIRECT destination still works ----
                ch.position(0);
                ByteBuffer direct = ByteBuffer.allocateDirect(8);
                n = ch.read(direct);
                check(n == 8, "direct read returned " + n);
                check(direct.get(0) == 0 && direct.get(7) == 7, "direct read got wrong bytes");

                // ---- 8. writing to a read-only CHANNEL is refused ----
                threw = false;
                try {
                    ch.write(ByteBuffer.wrap(new byte[4]));
                } catch (NonWritableChannelException e) {
                    threw = true;
                }
                check(threw, "write on a read-only channel did not throw");
            }

            // ---- 9. a write-only channel refuses reads ----
            try (FileChannel wo = FileChannel.open(tmp, StandardOpenOption.WRITE)) {
                boolean threw = false;
                try {
                    wo.read(ByteBuffer.allocate(4));
                } catch (NonReadableChannelException e) {
                    threw = true;
                }
                check(threw, "read on a write-only channel did not throw");
            }

            // ---- 10. an APPEND channel writes at the END, not at the cursor ----
            //
            // The fast path refuses an append descriptor precisely because
            // `write0` seeks to end for one and a naive fast path would not.
            // A refusal that failed to fire lands these bytes at offset 0.
            File f = tmp.toFile();
            try (FileOutputStream fos = new FileOutputStream(f, true);
                    FileChannel app = fos.getChannel()) {
                int n = app.write(ByteBuffer.wrap(new byte[] {(byte) 0xAB, (byte) 0xCD}));
                check(n == 2, "append write returned " + n);
            }
            byte[] all = Files.readAllBytes(tmp);
            check(all.length == 258, "file length after append = " + all.length);
            check(all[0] == 0, "append wrote over offset 0");
            check(all[256] == (byte) 0xAB && all[257] == (byte) 0xCD, "append bytes are misplaced");

            // ---- 11. a CLOSED channel refuses every operation ----
            FileChannel closed = FileChannel.open(tmp, StandardOpenOption.READ);
            closed.close();
            for (String what : new String[] {"read", "position", "size"}) {
                boolean threw = false;
                try {
                    switch (what) {
                        case "read" -> closed.read(ByteBuffer.allocate(4));
                        case "position" -> closed.position();
                        default -> closed.size();
                    }
                } catch (ClosedChannelException e) {
                    threw = true;
                }
                check(threw, what + "() on a closed channel did not throw ClosedChannelException");
            }

            // ---- 12. RandomAccessFile's channel shares the file position ----
            try (RandomAccessFile raf = new RandomAccessFile(f, "rw")) {
                raf.seek(64);
                FileChannel rc = raf.getChannel();
                check(rc.position() == 64, "RAF channel position = " + rc.position());
                ByteBuffer b = ByteBuffer.allocate(4);
                int n = rc.read(b);
                check(n == 4, "RAF channel read returned " + n);
                check(b.get(0) == 64, "RAF channel read got the wrong byte");
                check(raf.getFilePointer() == 68, "RAF pointer = " + raf.getFilePointer());
            }

            // Observables, not just a verdict: a run that asserted fewer
            // things than the oracle would otherwise diff identically.
            System.out.println("CK RFileChannelFastIo checks=" + checks);
            System.out.println("CK RFileChannelFastIo file_len=" + all.length);
            System.out.println("CK RFileChannelFastIo tail=" + (all[256] & 0xFF)
                    + "," + (all[257] & 0xFF));
            long sum = 0;
            for (int i = 0; i < 256; i++) {
                sum += all[i] & 0xFF;
            }
            System.out.println("CK RFileChannelFastIo ramp_sum=" + sum);
            System.out.println("PASS RFileChannelFastIo (" + checks + " checks)");
        } finally {
            Files.deleteIfExists(tmp);
        }
    }
}
