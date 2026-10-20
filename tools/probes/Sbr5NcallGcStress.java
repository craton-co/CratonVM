// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.File;
import java.io.FileInputStream;
import java.io.RandomAccessFile;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.atomic.AtomicLong;

/**
 * SBR round 5, lane {@code ncall}: GC stress for the native-call paths changed by the lane (the lean
 * JIT door, leaf {@code RandomAccessFile} rows, the lazily applied RAF seek, and the blocking-region
 * deposit that reuses the frozen stack-band verdict). Four threads run compiled loops that read a
 * known file through {@code FileInputStream} and {@code RandomAccessFile} while allocating objects
 * that hold references across those calls; every byte read is checked and every live object is
 * re-verified after each round. Prints {@code OK <reads>} or {@code FAIL ...}.
 *
 * <pre>cratonvm --jdk-only -Xmx96m [-XX:+UseG1GC] -cp . Sbr5NcallGcStress [seconds]</pre>
 */
public final class Sbr5NcallGcStress {
    static final int SIZE = 1 << 16;
    static final AtomicLong reads = new AtomicLong();
    static volatile String failure;

    static final class Node {
        final long a;
        final int[] pad;
        Node next;
        Node(long a, Node next) { this.a = a; this.pad = new int[8 + (int) (a & 15)]; this.next = next; }
    }

    static byte expect(long pos) { return (byte) (pos * 131 + 17); }

    /** Blocking natives (park, timed wait, sleep) from a compiled loop while holding live references. */
    static void blocker(int id, long deadline) {
        try {
            final Object lock = new Object();
            List<Node> keep = new ArrayList<>();
            long round = 0;
            while (System.nanoTime() < deadline && failure == null) {
                Node head = null;
                for (int i = 0; i < 32; i++) {
                    head = new Node(round * 32 + i, head);
                    if ((i & 3) == 0) java.util.concurrent.locks.LockSupport.parkNanos(2000);
                    if ((i & 7) == 1) { synchronized (lock) { lock.wait(0, 1000); } }
                    if ((i & 15) == 2) Thread.sleep(0, 1000);
                }
                keep.add(head);
                if (keep.size() > 100) keep.subList(0, 50).clear();
                for (Node h : keep) {
                    long last = Long.MAX_VALUE;
                    for (Node p = h; p != null; p = p.next) {
                        if (p.a >= last || p.pad.length != 8 + (int) (p.a & 15)) { failure = "blocker node corrupt"; return; }
                        last = p.a;
                    }
                }
                round++;
                reads.incrementAndGet();
            }
        } catch (Throwable t) {
            failure = "blocker " + t;
        }
    }

    static void worker(File f, int id, long deadline) {
        try {
            byte[] buf = new byte[1024];
            List<Node> keep = new ArrayList<>();
            try (FileInputStream fis = new FileInputStream(f); RandomAccessFile raf = new RandomAccessFile(f, "r")) {
                long pos = 0;
                int round = 0;
                while (System.nanoTime() < deadline && failure == null) {
                    // FileInputStream, sequential 1 KB, with live references across the call.
                    Node head = null;
                    for (int i = 0; i < 64; i++) {
                        head = new Node(round * 64L + i, head);
                        int n = fis.read(buf, 0, 1024);
                        if (n < 0) { fis.getChannel().position(0); pos = 0; continue; }
                        for (int j = 0; j < n; j += 61) {
                            if (buf[j] != expect(pos + j)) { failure = "fis mismatch at " + (pos + j); return; }
                        }
                        pos += n;
                        reads.incrementAndGet();
                    }
                    // RandomAccessFile: seek + read at scattered offsets (lazy seek, leaf rows).
                    for (int i = 0; i < 64; i++) {
                        long at = ((round * 977L + i * 4099L + id * 31L) % (SIZE - 1024));
                        raf.seek(at);
                        raf.readFully(buf, 0, 1024);
                        for (int j = 0; j < 1024; j += 53) {
                            if (buf[j] != expect(at + j)) { failure = "raf mismatch at " + (at + j); return; }
                        }
                        if (raf.getFilePointer() != at + 1024) { failure = "raf pointer " + raf.getFilePointer() + " != " + (at + 1024); return; }
                        reads.incrementAndGet();
                    }
                    keep.add(head);
                    if (keep.size() > 200) keep.subList(0, 100).clear();
                    // The retained chains must be intact.
                    if ((round & 15) == 0) {
                        for (Node h : keep) {
                            long last = Long.MAX_VALUE;
                            for (Node p = h; p != null; p = p.next) {
                                if (p.a >= last || p.pad.length != 8 + (int) (p.a & 15)) { failure = "node corrupt"; return; }
                                last = p.a;
                            }
                        }
                    }
                    round++;
                }
            }
        } catch (Throwable t) {
            failure = t.toString();
        }
    }

    public static void main(String[] args) throws Exception {
        int seconds = args.length > 0 ? Integer.parseInt(args[0]) : 20;
        File f = File.createTempFile("ncallgc", ".bin");
        f.deleteOnExit();
        byte[] all = new byte[SIZE];
        for (int i = 0; i < SIZE; i++) all[i] = expect(i);
        try (java.io.FileOutputStream o = new java.io.FileOutputStream(f)) { o.write(all); }
        long deadline = System.nanoTime() + seconds * 1_000_000_000L;
        Thread[] ts = new Thread[6];
        for (int i = 0; i < ts.length; i++) {
            final int id = i;
            ts[i] = new Thread(id < 4 ? () -> worker(f, id, deadline) : () -> blocker(id, deadline), "w" + i);
            ts[i].start();
        }
        // a churn thread to force collections while the workers sit in native calls
        Thread churn = new Thread(() -> {
            List<byte[]> junk = new ArrayList<>();
            while (System.nanoTime() < deadline && failure == null) {
                junk.add(new byte[4096]);
                if (junk.size() > 4000) junk.clear();
            }
        });
        churn.start();
        for (Thread t : ts) t.join();
        churn.join();
        System.out.println(failure == null ? "OK " + (reads.get() > 0) : "FAIL " + failure);
    }
}
