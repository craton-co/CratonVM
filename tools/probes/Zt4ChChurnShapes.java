// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zt t4/ch (2026-10-19): where `G1ChurnPauseProbe`'s CPU goes.
//
// `G1ChurnPauseProbe` (no -Xmx, 24 MiB live, 200 rounds of 4 MiB garbage)
// runs ~7-10x HotSpot's CPU on ZGC. By arithmetic it is ONE warm-up
// collection on a ~4 GiB default heap (10 % of the soft maximum is ~406 MiB;
// the run allocates ~945 MB in all), so the collector's pauses cannot carry
// the gap; the candidates are the mutator loop's shape (the whole loop lives
// in `main`, ONE activation, followed by a string concatenation: an OSR body,
// see the "string concat after a loop forces single-pass OSR" lesson) and the
// work outside the loop (boot, compiler threads, the concat's bootstrap).
// This probe splits them with one binary:
//
//   main    -- G1ChurnPauseProbe's loop, verbatim, inline in `main` (the
//              shape under test; its checksum equals G1ChurnPauseProbe's);
//   method  -- the same work, one round per call of a static method (200
//              activations: the method compiles at method entry, not OSR);
//   noalloc -- `method` without the garbage stream (the walk alone).
//
// stdout is deterministic (shape, sizes, checksum) and byte-compared with
// HotSpot's; the timings go to stderr: wall and the MAIN THREAD's CPU
// (ThreadMXBean), so process CPU (from the OS) minus thread CPU is the work
// on every other thread (collector workers, compiler threads, boot).
//
// Usage: Zt4ChChurnShapes <main|method|noalloc> [liveMiB=24] [rounds=200]
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;

public final class Zt4ChChurnShapes {

    static final class Node {
        Node next;
        final byte[] payload;
        int tag;

        Node(Node next, int payloadBytes, int tag) {
            this.next = next;
            this.payload = new byte[payloadBytes];
            this.tag = tag;
        }
    }

    static final int NODE_BYTES = 256;

    /** One round of G1ChurnPauseProbe's loop body; returns the checksum delta. */
    static long round(Node head, int r, boolean alloc) {
        long checksum = 0;
        if (alloc) {
            for (int i = 0; i < (4 * 1024 * 1024) / NODE_BYTES; i++) {
                Node dead = new Node(null, NODE_BYTES - 32, i);
                checksum += dead.payload.length + dead.tag;
            }
        }
        Node cur = head;
        int walked = 0;
        while (cur != null && walked < 4096) {
            cur.tag = cur.tag + r;
            checksum += cur.tag;
            cur = cur.next;
            walked++;
        }
        return checksum;
    }

    public static void main(String[] args) {
        String shape = args.length > 0 ? args[0] : "main";
        int liveMiB = args.length > 1 ? Integer.parseInt(args[1]) : 24;
        int rounds = args.length > 2 ? Integer.parseInt(args[2]) : 200;
        if (!shape.equals("main") && !shape.equals("method") && !shape.equals("noalloc")) {
            throw new IllegalArgumentException("shape must be main, method or noalloc: " + shape);
        }
        final int nodeBytes = NODE_BYTES;
        final int liveNodes = (liveMiB * 1024 * 1024) / nodeBytes;
        Node head = null;
        for (int i = 0; i < liveNodes; i++) {
            head = new Node(head, nodeBytes - 32, i);
        }
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        long checksum = 0;
        long garbageBytes = 0;
        long cpu0 = mx.getCurrentThreadCpuTime();
        long start = System.nanoTime();
        if (shape.equals("main")) {
            // G1ChurnPauseProbe's loop, verbatim.
            for (int r = 0; r < rounds; r++) {
                for (int i = 0; i < (4 * 1024 * 1024) / nodeBytes; i++) {
                    Node dead = new Node(null, nodeBytes - 32, i);
                    checksum += dead.payload.length + dead.tag;
                    garbageBytes += nodeBytes;
                }
                Node cur = head;
                int walked = 0;
                while (cur != null && walked < 4096) {
                    cur.tag = cur.tag + r;
                    checksum += cur.tag;
                    cur = cur.next;
                    walked++;
                }
            }
        } else {
            boolean alloc = shape.equals("method");
            for (int r = 0; r < rounds; r++) {
                checksum += round(head, r, alloc);
                if (alloc) {
                    garbageBytes += (long) ((4 * 1024 * 1024) / nodeBytes) * nodeBytes;
                }
            }
        }
        long wallMs = (System.nanoTime() - start) / 1_000_000L;
        long cpuMs = (mx.getCurrentThreadCpuTime() - cpu0) / 1_000_000L;
        System.out.println("Zt4ChChurnShapes shape=" + shape
                + " live=" + liveMiB + "MiB"
                + " liveNodes=" + liveNodes
                + " rounds=" + rounds
                + " garbageMiB=" + (garbageBytes / (1024 * 1024))
                + " checksum=" + checksum);
        System.err.println("zt4ch shape=" + shape + " wall_ms=" + wallMs
                + " main_thread_cpu_ms=" + cpuMs);
    }
}
