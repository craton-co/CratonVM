// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zr z9/bo (2026-10-13): every compiled site kind that sets a ZGC start bit
// inline, kept live across collections and re-verified.
//
// CRATONVM_ZGC_JIT_SHORT_ANNOUNCE=1 changes the instructions of the inline
// start-bit store in BOTH tiers: the `new` of a post-init no-op class, the
// primitive `newarray`, the `anewarray`, the `Integer.valueOf` / `Long.valueOf`
// box prefixes (values outside the cache), and the call-free pair of the
// inline `StringBuilder.toString()`. A bit set on the wrong word, or not at
// all, leaves a live object out of the start registry: the next collection
// frees it and a later read finds another object's bytes. So the kernels fill
// a ring of objects whose contents are a function of their index, a quarter
// of the ring is replaced per round, collections run between rounds (the
// churn, plus System.gc() every 8th round), and every round re-verifies the
// WHOLE ring: class, length and contents.
//
// Usage: java -Xmx256m Zr9BoShortAnnounceSites [rounds=400] [ring=4096]
// Output: one `check` line (must equal HotSpot's), then an informational
// `timing` line (ThreadMXBean CPU of the main thread).
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;

public final class Zr9BoShortAnnounceSites {
    static final class Node {
        final long id;
        final int[] payload;
        Node next;

        Node(long id, int[] payload) {
            this.id = id;
            this.payload = payload;
        }
    }

    static final class Plain {
        long a;
        int b;
    }

    static Object make(long id) {
        switch ((int) (id % 6)) {
            case 0: {
                Plain p = new Plain();
                p.a = id * 31;
                p.b = (int) id;
                return p;
            }
            case 1: {
                int n = (int) (id % 13);
                int[] a = new int[n];
                for (int k = 0; k < n; k++) {
                    a[k] = (int) (id + k);
                }
                return a;
            }
            case 2: {
                Object[] a = new Object[(int) (id % 5) + 1];
                a[0] = Long.valueOf(id + 1_000_000L);
                return a;
            }
            case 3:
                return Integer.valueOf((int) (id + 100_000));
            case 4: {
                StringBuilder sb = new StringBuilder();
                sb.append('s').append(id);
                return sb.toString();
            }
            default: {
                int[] a = new int[2];
                a[0] = (int) id;
                a[1] = (int) (id >>> 1);
                return new Node(id, a);
            }
        }
    }

    static long verify(Object o, long id) {
        switch ((int) (id % 6)) {
            case 0: {
                Plain p = (Plain) o;
                return (p.a == id * 31 && p.b == (int) id) ? 1 : -1_000_000;
            }
            case 1: {
                int[] a = (int[]) o;
                if (a.length != (int) (id % 13)) {
                    return -1_000_000;
                }
                for (int k = 0; k < a.length; k++) {
                    if (a[k] != (int) (id + k)) {
                        return -1_000_000;
                    }
                }
                return 1;
            }
            case 2: {
                Object[] a = (Object[]) o;
                return (a.length == (int) (id % 5) + 1 && ((Long) a[0]) == id + 1_000_000L) ? 1 : -1_000_000;
            }
            case 3:
                return ((Integer) o) == (int) (id + 100_000) ? 1 : -1_000_000;
            case 4:
                return ("s" + id).equals(o) ? 1 : -1_000_000;
            default: {
                Node n = (Node) o;
                return (n.id == id && n.payload.length == 2 && n.payload[0] == (int) id
                        && n.payload[1] == (int) (id >>> 1)) ? 1 : -1_000_000;
            }
        }
    }

    static void fill(Object[] ring, long[] ids, long first, int from, int count) {
        for (int k = 0; k < count; k++) {
            int slot = (from + k) % ring.length;
            long id = first + k;
            ring[slot] = make(id);
            ids[slot] = id;
        }
    }

    static long verifyAll(Object[] ring, long[] ids) {
        long ok = 0;
        for (int i = 0; i < ring.length; i++) {
            ok += verify(ring[i], ids[i]);
        }
        return ok;
    }

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 400;
        int size = args.length > 1 ? Integer.parseInt(args[1]) : 4096;
        Object[] ring = new Object[size];
        long[] ids = new long[size];
        long next = 0;
        // Warm-up: compile every kernel before the counted rounds.
        for (int w = 0; w < 200; w++) {
            fill(ring, ids, next, 0, size);
            next += size;
            verifyAll(ring, ids);
        }
        fill(ring, ids, next, 0, size);
        next += size;
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        long cpu0 = mx.getCurrentThreadCpuTime();
        long okSum = 0;
        long bad = 0;
        long garbage = 0;
        for (int r = 0; r < rounds; r++) {
            int quarter = size / 4;
            fill(ring, ids, next, (r * quarter) % size, quarter);
            next += quarter;
            // Churn: short-lived objects of every site kind, so collections run.
            for (int g = 0; g < 4 * size; g++) {
                Object o = make(next + g);
                garbage += (o.hashCode() & 1);
            }
            if (r % 8 == 7) {
                System.gc();
            }
            long ok = verifyAll(ring, ids);
            if (ok != size) {
                bad++;
            }
            okSum += ok;
        }
        long cpu = mx.getCurrentThreadCpuTime() - cpu0;
        System.out.println("check rounds=" + rounds + " ring=" + size + " bad_rounds=" + bad
                + " verified=" + okSum + " ok=" + (bad == 0 && okSum == (long) rounds * size));
        System.out.println("timing cpu_ms=" + (cpu / 1_000_000) + " garbage_bit=" + (garbage > 0));
    }
}
