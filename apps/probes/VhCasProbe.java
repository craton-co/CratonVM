// `VarHandle.compareAndSet` on its own, with no read in the loop.
//
// `HibfixVarHandleProbe`'s CAS rows are COMPOSITES: each iteration does a
// `VarHandle.get` and then the CAS, so its "VarHandle.CAS reference 231 ns" is
// a 77 ns read plus a ~154 ns CAS. That is fine for a ratio against HotSpot,
// whose rows compose the same way, and useless for attributing the CAS's own
// cost or for reading a profile. This probe carries the expected value in a
// Java local instead, so the timed loop contains exactly one VarHandle
// operation.
//
// Four arms, because the CAS has four shapes worth separating:
//
//   * reference, SUCCEEDING  — what `CompletableFuture.tryPushStack` does on an
//     uncontended push, and the row the known-issues page is about;
//   * reference, FAILING     — the same call with a stale `expected`, which
//     skips the store and the post write barrier but still pays the SATB
//     pre-barrier and the whole dispatch;
//   * int, SUCCEEDING        — the same path with a primitive payload, so the
//     difference between the two bounds the reference-specific work;
//   * plain volatile store   — the floor, for scale.
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

public class VhCasProbe {

    static class Node { Node next; int tag; }

    volatile Node ref;
    volatile int num;

    static final VarHandle REF;
    static final VarHandle NUM;
    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            REF = l.findVarHandle(VhCasProbe.class, "ref", Node.class);
            NUM = l.findVarHandle(VhCasProbe.class, "num", int.class);
        } catch (Exception e) { throw new ExceptionInInitializerError(e); }
    }

    static void report(String what, long ops, long ns) {
        System.out.printf("@@VHCAS %-26s ops=%d ns_per_op=%.1f total_ms=%d%n",
                what, ops, (double) ns / ops, ns / 1_000_000);
    }

    public static void main(String[] args) {
        int iters = Integer.getInteger("probe.iters", 5_000_000);
        VhCasProbe p = new VhCasProbe();
        Node a = new Node(), b = new Node(), stale = new Node();
        p.ref = a;
        p.num = 0;

        long ok = 0, failed = 0;

        // Warm every arm before timing any of it.
        for (int i = 0; i < 200_000; i++) {
            Node cur = (i & 1) == 0 ? a : b;
            Node nxt = (i & 1) == 0 ? b : a;
            if (REF.compareAndSet(p, cur, nxt)) ok++;
            if (REF.compareAndSet(p, stale, a)) failed++;
            if (NUM.compareAndSet(p, i, i + 1)) ok++;
            REF.set(p, nxt);
        }
        p.ref = a;
        p.num = 0;

        long t;

        // Reference CAS that SUCCEEDS. `cur` is tracked in a local, so the loop
        // holds one VarHandle call and no read.
        t = System.nanoTime();
        {
            Node cur = a;
            for (int i = 0; i < iters; i++) {
                Node nxt = (cur == a) ? b : a;
                if (REF.compareAndSet(p, cur, nxt)) ok++;
                cur = nxt;
            }
        }
        report("CAS reference success", iters, System.nanoTime() - t);

        // Reference CAS that FAILS: `stale` was never stored, so every call
        // compares unequal and stores nothing.
        t = System.nanoTime();
        for (int i = 0; i < iters; i++) {
            if (REF.compareAndSet(p, stale, a)) failed++;
        }
        report("CAS reference fail", iters, System.nanoTime() - t);

        // int CAS that SUCCEEDS, same shape, primitive payload.
        t = System.nanoTime();
        {
            int cur = p.num;
            for (int i = 0; i < iters; i++) {
                if (NUM.compareAndSet(p, cur, cur + 1)) ok++;
                cur = cur + 1;
            }
        }
        report("CAS int success", iters, System.nanoTime() - t);

        // The floor: a volatile reference store through the same VarHandle.
        t = System.nanoTime();
        for (int i = 0; i < iters; i++) {
            REF.set(p, (i & 1) == 0 ? a : b);
        }
        report("set reference (floor)", iters, System.nanoTime() - t);

        System.out.println("@@VHCAS done ok=" + ok + " failed=" + failed
                + " num=" + p.num + " refIsA=" + (p.ref == a));
    }
}
