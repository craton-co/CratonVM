// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zgc round zr, lane mt (2026-10-13, after zr z4/rv4's mark-pool page item 1).
//
// An IDLE process that has already run parallel marks: the persistent STW mark
// pool exists and is parked between cycles. Until zr z3/mt's untimed park each
// parked worker woke every 5 ms (~200/s/worker), so an idle ZGC VM burned CPU
// with nothing to do. Measure the process's user+sys CPU with /usr/bin/time:
// the default (PARMARK=auto, untimed park) must cost about what
// CRATONVM_ZGC_PARMARK=0 (no pool) costs; CRATONVM_ZGC_MARK_IDLE_PARK_UNTIMED=0
// is the positive control (the poll: measurably more CPU).
//
// Shape: build a live set big enough that `auto` picks >1 worker on a large
// heap (run with -Xmx4g), System.gc() twice so the pool is spawned and has run
// two cycles, then sleep `seconds` with nothing allocating.
//
// Usage: Zr3MtIdlePool [seconds]
// Output: ZR3MT-IDLE seconds=<s> live=<n> ok
public class Zr3MtIdlePool {
    static final class Node {
        Node next;
        long pad;
    }

    static Node live;

    public static void main(String[] args) throws Exception {
        int seconds = args.length > 0 ? Integer.parseInt(args[0]) : 10;
        Node head = null;
        int n = 500_000;
        for (int i = 0; i < n; i++) {
            Node x = new Node();
            x.pad = i;
            x.next = head;
            head = x;
        }
        live = head;
        System.gc();
        System.gc();
        Thread.sleep(seconds * 1000L);
        long count = 0;
        for (Node x = live; x != null; x = x.next) count++;
        System.out.println("ZR3MT-IDLE seconds=" + seconds + " live=" + count + " ok");
    }
}
