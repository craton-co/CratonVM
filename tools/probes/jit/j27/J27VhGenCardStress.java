// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// j27 wave 2 (lane VHCOMP; CRATONVM_JIT_VH_CONST_GEN_CARD_EDGE): old holders
// repointed at fresh young nodes THROUGH CONSTANT VarHandles (set, setVolatile,
// setRelease, compareAndSet, compareAndExchange, getAndSet) by several threads
// across many young pauses. A lost card shows as a wrong id / payload or a crash.
// The twin of tools/probes/VhcaeG1Stress.java, whose stores are putfields.
// Expected (HotSpot 25 and CratonVM, every collector):
//   fact vh-gen-stress-ok true
//   fact vh-gen-stress-sum <the line HotSpot prints>
//   java J27VhGenCardStress
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

public class J27VhGenCardStress {
    static final class Node {
        final int id; final long[] payload; Node next;
        Node(int id) { this.id = id; payload = new long[8]; for (int i = 0; i < 8; i++) payload[i] = (long) id * 31 + i; }
    }
    static final class Holder { volatile Node f; Node g; }
    static final VarHandle F, G;
    static {
        try {
            F = MethodHandles.lookup().findVarHandle(Holder.class, "f", Node.class);
            G = MethodHandles.lookup().findVarHandle(Holder.class, "g", Node.class);
        } catch (ReflectiveOperationException e) { throw new ExceptionInInitializerError(e); }
    }

    static final int HOLDERS = 2048, ROUNDS = Integer.getInteger("j27.rounds", 120), THREADS = 4;
    static final Holder[][] holders = new Holder[THREADS][];
    static volatile Object garbage;

    static void store(Holder h, Node n, int mode) {
        switch (mode) {
            case 0 -> F.setVolatile(h, n);
            case 1 -> { Node cur = (Node) F.getVolatile(h); if (!F.compareAndSet(h, cur, n)) F.setVolatile(h, n); }
            case 2 -> { Node cur = (Node) F.getVolatile(h); if (F.compareAndExchange(h, cur, n) != cur) F.setVolatile(h, n); }
            case 3 -> F.getAndSet(h, n);
            default -> F.setRelease(h, n);
        }
        G.set(h, n.next);
    }

    static long check(Holder[] hs, int round) {
        long s = 0;
        for (int i = 0; i < hs.length; i++) {
            Node n = hs[i].f;
            int want = round * HOLDERS + i;
            if (n == null || n.id != want) return -1 - i;
            for (int k = 0; k < 8; k++) if (n.payload[k] != (long) want * 31 + k) return -100000 - i;
            Node m = hs[i].g;
            if (m == null || m.id != -want || n.next != m) return -200000 - i;
            s += n.id;
        }
        return s;
    }

    public static void main(String[] a) throws Exception {
        for (int t = 0; t < THREADS; t++) { holders[t] = new Holder[HOLDERS]; for (int i = 0; i < HOLDERS; i++) holders[t][i] = new Holder(); }
        for (int i = 0; i < 3; i++) System.gc();
        final long[] sums = new long[THREADS];
        final boolean[] ok = new boolean[THREADS];
        Thread[] ts = new Thread[THREADS];
        for (int t = 0; t < THREADS; t++) {
            final int tt = t;
            ts[t] = new Thread(() -> {
                Holder[] hs = holders[tt];
                boolean good = true; long total = 0;
                for (int r = 0; r < ROUNDS; r++) {
                    for (int i = 0; i < hs.length; i++) {
                        Node n = new Node(r * HOLDERS + i);
                        n.next = new Node(-(r * HOLDERS + i));
                        store(hs[i], n, (i + r) % 5);
                        if ((i & 15) == 0) garbage = new byte[2048];
                    }
                    for (int g = 0; g < 200; g++) garbage = new long[256];
                    long c = check(hs, r);
                    if (c < 0) { good = false; System.out.println("worker-failed t=" + tt + " r=" + r + " code=" + c); break; }
                    total += c;
                }
                sums[tt] = total; ok[tt] = good;
            });
            ts[t].start();
        }
        for (Thread t : ts) t.join();
        long all = 0; boolean good = true;
        for (int t = 0; t < THREADS; t++) { all += sums[t]; good &= ok[t]; }
        System.out.println("fact vh-gen-stress-ok " + good);
        System.out.println("fact vh-gen-stress-sum " + all);
    }
}
