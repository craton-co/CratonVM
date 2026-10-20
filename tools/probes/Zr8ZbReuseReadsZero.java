// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zr z8/zb (2026-10-13): memory the ZGC sweep reclaimed reads as a fresh object
// when it is handed out again, with the sweep's dead-header store OFF (the
// default since z8/zb, `CRATONVM_ZGC_SWEEP_SKIP_DEAD_HEADERS`; `=0` restores
// the store).
//
// Every object this probe drops has a dirty body (a marker written into every
// element / field) and, with the store off, a dirty header. A hand-out path
// that relied on the sweep having zeroed the header -- or that zeroed only part
// of a recycled span -- shows up here as a fresh object with a non-default
// element, a wrong length, or a wrong class. A ring keeps about one object in
// eight alive, so the sweep leaves fragmented spans of every size and the free
// list, the TLAB carve and the slide all serve recycled memory.
//
// Deterministic (fixed iteration counts, seeded lengths): the CHECK line must
// equal HotSpot's (`java -XX:+UseZGC -Xmx64m Zr8ZbReuseReadsZero`).
//
// Usage: java -Xmx64m Zr8ZbReuseReadsZero [rounds=40] [perRound=200000]
public final class Zr8ZbReuseReadsZero {
    static final class Node {
        long a;
        long b;
        Object next;
        int c;
    }

    static long seed = 0x9E3779B97F4A7C15L;

    static int nextLen(int mod) {
        seed ^= seed << 13;
        seed ^= seed >>> 7;
        seed ^= seed << 17;
        return (int) ((seed >>> 33) % mod);
    }

    static long bad;
    static long checkedElems;

    static void checkLong(long[] a, int want) {
        if (a.length != want) {
            bad++;
        }
        for (long v : a) {
            checkedElems++;
            if (v != 0L) {
                bad++;
            }
        }
        java.util.Arrays.fill(a, 0xDEADBEEFCAFEL);
    }

    static void checkRef(Object[] a, int want) {
        if (a.length != want) {
            bad++;
        }
        for (Object v : a) {
            checkedElems++;
            if (v != null) {
                bad++;
            }
        }
        java.util.Arrays.fill(a, a);
    }

    static void checkNode(Node n) {
        checkedElems += 4;
        if (n.a != 0 || n.b != 0 || n.next != null || n.c != 0) {
            bad++;
        }
        n.a = -1;
        n.b = -2;
        n.next = n;
        n.c = -3;
    }

    static void round(Object[] ring, int perRound, int r) {
        for (int i = 0; i < perRound; i++) {
            Object o;
            switch (i % 3) {
                case 0 -> {
                    int len = nextLen(97);
                    long[] a = new long[len];
                    checkLong(a, len);
                    o = a;
                }
                case 1 -> {
                    int len = nextLen(61);
                    Object[] a = new Object[len];
                    checkRef(a, len);
                    o = a;
                }
                default -> {
                    Node n = new Node();
                    checkNode(n);
                    o = n;
                }
            }
            if ((i & 7) == 0) {
                ring[(i >>> 3) % ring.length] = o;
            }
        }
        // The survivors must still hold the markers written into them.
        for (Object o : ring) {
            if (o instanceof long[] a) {
                for (long v : a) {
                    if (v != 0xDEADBEEFCAFEL) {
                        bad++;
                    }
                }
            } else if (o instanceof Object[] a) {
                for (Object v : a) {
                    if (v != a) {
                        bad++;
                    }
                }
            } else if (o instanceof Node n) {
                if (n.a != -1 || n.b != -2 || n.next != n || n.c != -3) {
                    bad++;
                }
            } else if (o != null) {
                bad++;
            }
        }
    }

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 40;
        int perRound = args.length > 1 ? Integer.parseInt(args[1]) : 200_000;
        Object[] ring = new Object[4096];
        for (int r = 0; r < rounds; r++) {
            round(ring, perRound, r);
        }
        System.out.println("Zr8ZbReuseReadsZero check rounds=" + rounds + " per_round=" + perRound
                + " elems=" + checkedElems + " bad=" + bad + " ok=" + (bad == 0));
    }
}
