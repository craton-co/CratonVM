// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.util.Arrays;
import java.util.concurrent.atomic.AtomicReferenceArray;

/**
 * gci i12/wb (2026-10-13): reference stores from COMPILED code against both
 * collectors' barriers, for {@code docs/known-issues/gc/gci-i12wb-jit-barrier-review-findings-20261013.md}.
 *
 * <p>Two modes, each printing ONE deterministic line (HotSpot prints the same):
 * <ul>
 *   <li>{@code gate [threads] [holders] [iters] [ring]} -- the frozen-peer SATB
 *       window (item 1). Every thread owns {@code holders} holders whose one field
 *       names a {@code Token}; in a hot loop it copies a random token of ANOTHER
 *       thread's holder into one of its own (an overwrite of a non-null field,
 *       the g1put gated arm's shape), now and then replaces one of its own with a
 *       fresh token (dropping the old one), and keeps a retained ring growing so
 *       concurrent cycles keep opening. A token freed while still reachable
 *       reads back a wrong magic. Prints
 *       {@code wb-gate threads=T holders=H iters=I corrupt=0}.</li>
 *   <li>{@code o2y [rounds] [slots]} -- old-to-young edges made by every
 *       compiled reference-store shape the review covers (item 6): putfield,
 *       volatile putfield, aastore, {@code AtomicReferenceArray.set} /
 *       {@code compareAndSet}, a constant {@code VarHandle}'s {@code setVolatile} /
 *       {@code compareAndSet}, {@code System.arraycopy} into an old array, and
 *       {@code clone} / {@code Arrays.copyOf} of an old array kept in an old
 *       holder. Each round stores fresh (young) tokens into long-lived (old)
 *       holders, churns enough to run young collections, and validates every
 *       slot by identity. Prints {@code wb-o2y rounds=R slots=S corrupt=0}.</li>
 * </ul>
 * Diagnostics go to stderr only. Usage: {@code GciI12WbBarrierProbe <gate|o2y> [args]}.
 */
public final class GciI12WbBarrierProbe {
    static final class Token {
        final int id;
        final int magic;
        final long[] body;

        Token(int id) {
            this.id = id;
            this.magic = id * 0x9E3779B1 ^ 0x5A5A5A5A;
            this.body = new long[4];
            this.body[id & 3] = id;
        }

        boolean valid() {
            return magic == (id * 0x9E3779B1 ^ 0x5A5A5A5A) && body != null && body.length == 4
                    && body[id & 3] == id;
        }
    }

    static final class Holder {
        Token f;
        volatile Token v;
    }

    static final VarHandle F;

    static {
        try {
            F = MethodHandles.lookup().findVarHandle(Holder.class, "f", Token.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    /** Escapes every churn allocation, so none is scalar-replaced. */
    static volatile Object churnSink;

    static int arg(String[] args, int i, int dflt) {
        return args.length > i ? Integer.parseInt(args[i]) : dflt;
    }

    /** xorshift32: deterministic per thread, allocation-free. */
    static int next(int x) {
        x ^= x << 13;
        x ^= x >>> 17;
        x ^= x << 5;
        return x;
    }

    /** About {@code kb} KiB of short-lived garbage: young collections. */
    static void churn(int kb) {
        for (int k = 0; k < kb; k++) {
            churnSink = new byte[1000];
        }
    }

    static long gate(int threads, int holders, int iters, int ring) throws InterruptedException {
        final Holder[][] all = new Holder[threads][holders];
        for (int t = 0; t < threads; t++) {
            for (int h = 0; h < holders; h++) {
                Holder o = new Holder();
                o.f = new Token(t * holders + h);
                all[t][h] = o;
            }
        }
        final long[] corrupt = new long[threads];
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int me = t;
            ts[t] = new Thread(() -> {
                Holder[] mine = all[me];
                Object[] retained = new Object[ring];
                int x = 0x1234567 + me * 7919;
                int fresh = threads * holders + me;
                long bad = 0;
                for (int i = 0; i < iters; i++) {
                    x = next(x);
                    int other = (me + 1 + ((x >>> 8) & 0x7fffffff) % Math.max(1, threads - 1)) % threads;
                    Token a = all[other][(x & 0x7fffffff) % holders].f;
                    if (a == null || !a.valid()) {
                        bad++;
                    } else {
                        mine[((x >>> 3) & 0x7fffffff) % holders].f = a;
                    }
                    if ((i & 63) == 0) {
                        mine[((x >>> 5) & 0x7fffffff) % holders].f = new Token(fresh);
                        fresh += threads;
                    }
                    if ((i & 15) == 0) {
                        retained[(i >>> 4) % ring] = new long[16];
                    }
                }
                for (Holder h : mine) {
                    if (h.f == null || !h.f.valid()) {
                        bad++;
                    }
                }
                corrupt[me] = bad;
            }, "wb-gate-" + t);
            ts[t].start();
        }
        for (Thread t : ts) {
            t.join();
        }
        long total = 0;
        for (long c : corrupt) {
            total += c;
        }
        return total;
    }

    /** Is {@code o} the token with id {@code id}? Counts 1 when it is not. */
    static long bad(Object o, int id) {
        if (!(o instanceof Token)) {
            return 1;
        }
        Token t = (Token) o;
        return (t.id == id && t.valid()) ? 0 : 1;
    }

    static long o2y(int rounds, int slots) {
        Holder[] holders = new Holder[slots];
        for (int i = 0; i < slots; i++) {
            holders[i] = new Holder();
        }
        Object[] oldArr = new Object[slots];
        AtomicReferenceArray<Token> ara = new AtomicReferenceArray<>(slots);
        Object[][] copies = new Object[2][];
        // Age the containers: several young collections promote them.
        churn(64 * 1024);
        long corrupt = 0;
        for (int r = 0; r < rounds; r++) {
            int base = r * slots;
            for (int i = 0; i < slots; i++) {
                Token t = new Token(base + i);
                switch (i & 7) {
                    case 0:
                        holders[i].f = t;
                        break;
                    case 1:
                        holders[i].v = t;
                        break;
                    case 2:
                        oldArr[i] = t;
                        break;
                    case 3:
                        ara.set(i, t);
                        break;
                    case 4:
                        F.setVolatile(holders[i], t);
                        break;
                    case 5: {
                        Token cur = (Token) F.get(holders[i]);
                        if (!F.compareAndSet(holders[i], cur, t)) {
                            corrupt++;
                        }
                        break;
                    }
                    case 6: {
                        Object[] one = new Object[] {t};
                        System.arraycopy(one, 0, oldArr, i, 1);
                        break;
                    }
                    default: {
                        Token cur = ara.get(i);
                        if (!ara.compareAndSet(i, cur, t)) {
                            corrupt++;
                        }
                        break;
                    }
                }
                if ((i & 15) == 0) {
                    churnSink = new long[8];
                }
            }
            copies[r & 1] = ((r & 2) == 0) ? oldArr.clone() : Arrays.copyOf(oldArr, slots);
            // Young collections between the stores and the reads: only a
            // remembered old->young edge keeps a token where it was stored.
            churn(8 * 1024);
            Object[] copy = copies[r & 1];
            for (int i = 0; i < slots; i++) {
                int id = base + i;
                switch (i & 7) {
                    case 0:
                    case 4:
                    case 5:
                        corrupt += bad(holders[i].f, id);
                        break;
                    case 1:
                        corrupt += bad(holders[i].v, id);
                        break;
                    case 2:
                    case 6:
                        corrupt += bad(oldArr[i], id);
                        corrupt += bad(copy[i], id);
                        break;
                    default:
                        corrupt += bad(ara.get(i), id);
                        break;
                }
            }
        }
        return corrupt;
    }

    public static void main(String[] args) throws InterruptedException {
        String mode = args.length > 0 ? args[0] : "o2y";
        String[] rest = args.length > 0 ? Arrays.copyOfRange(args, 1, args.length) : new String[0];
        long t0 = System.nanoTime();
        if (mode.equals("gate")) {
            int threads = arg(rest, 0, 8);
            int holders = arg(rest, 1, 4096);
            int iters = arg(rest, 2, 4_000_000);
            int ring = arg(rest, 3, 20_000);
            long corrupt = gate(threads, holders, iters, ring);
            System.out.println("wb-gate threads=" + threads + " holders=" + holders + " iters=" + iters
                    + " corrupt=" + corrupt);
        } else if (mode.equals("o2y")) {
            int rounds = arg(rest, 0, 100);
            int slots = arg(rest, 1, 8192);
            long corrupt = o2y(rounds, slots);
            System.out.println("wb-o2y rounds=" + rounds + " slots=" + slots + " corrupt=" + corrupt);
        } else {
            System.out.println("usage: GciI12WbBarrierProbe <gate|o2y> [args]");
            return;
        }
        System.err.println("[gci-i12wb] mode=" + mode + " ms=" + (System.nanoTime() - t0) / 1_000_000);
    }
}
