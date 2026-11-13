// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company
//
// Mutator hiccups under a large, fragmenting live set: what ZGC's concurrent
// relocation buys over the in-pause slide. Builds `liveMb` of small linked
// objects, then replaces a slice each step so regions fragment; the main
// thread records the longest gaps between steps (pause + time to safepoint).
//
// Usage: ZgcRelocHiccup [liveMb=400] [seconds=20]
// Prints `fact` lines (checksum) and `time` lines (hiccup max / p99 / steps).
import java.util.Arrays;

public class ZgcRelocHiccup {
    static final class Node {
        Node next;
        final long v;
        final byte[] pad;
        Node(long v, Node next) {
            this.v = v;
            this.next = next;
            this.pad = new byte[48 + (int) (v & 63)];
        }
    }

    public static void main(String[] args) {
        int liveMb = args.length > 0 ? Integer.parseInt(args[0]) : 400;
        int seconds = args.length > 1 ? Integer.parseInt(args[1]) : 20;
        int chains = 4096;
        int perChain = (int) ((liveMb * 1024L * 1024L) / 160 / chains);
        Node[] heads = new Node[chains];
        long seq = 0;
        for (int c = 0; c < chains; c++) {
            for (int i = 0; i < perChain; i++) heads[c] = new Node(seq++, heads[c]);
        }
        long[] gaps = new long[1 << 20];
        int n = 0;
        long end = System.nanoTime() + seconds * 1_000_000_000L;
        long last = System.nanoTime();
        long check = 0;
        int step = 0;
        while (last < end) {
            // Rebuild one chain: its old nodes die, scattered among survivors.
            int c = (int) Long.remainderUnsigned(step * 2654435761L, chains);
            Node h = null;
            for (int i = 0; i < perChain; i++) h = new Node(seq++, h);
            heads[c] = h;
            for (Node x = heads[(c + 1) % chains]; x != null; x = x.next) check += x.v & 7;
            long now = System.nanoTime();
            if (n < gaps.length) gaps[n++] = now - last;
            last = now;
            step++;
        }
        long[] g = Arrays.copyOf(gaps, n);
        Arrays.sort(g);
        long sum = 0;
        for (Node x : heads) for (; x != null; x = x.next) sum++;
        System.out.println("fact live-nodes " + sum);
        System.out.println("time steps " + n);
        System.out.println("time p50-us " + g[n / 2] / 1000);
        System.out.println("time p99-us " + g[(int) (n * 0.99)] / 1000);
        System.out.println("time p999-us " + g[(int) (n * 0.999)] / 1000);
        System.out.println("time max-us " + g[n - 1] / 1000);
        System.out.println("time check " + (check & 1));
    }
}
