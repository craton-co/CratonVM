// How fast is a VarHandle CAS on a REFERENCE field?
//
// A clean profile of CompletableFuture composition (HibfixComposeProbe2,
// 412ms HotSpot vs 359s CratonVM) puts 34% of the time in
// CompletableFuture.tryPushStack, which is exactly:
//
//     NEXT.set(c, h);                       // VarHandle set, reference field
//     return STACK.compareAndSet(this, h, c);  // VarHandle CAS, reference field
//
// Each thread owns its futures there, so that CAS is UNCONTENDED and should
// succeed on the first try. 34% in an uncontended CAS points at the primitive,
// not the algorithm. This times each primitive on its own.
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicReference;

public class HibfixVarHandleProbe {

    static class Node { Node next; int tag; }

    volatile Node ref;
    volatile int num;
    Node plain;

    static final VarHandle REF;
    static final VarHandle NUM;
    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            REF = l.findVarHandle(HibfixVarHandleProbe.class, "ref", Node.class);
            NUM = l.findVarHandle(HibfixVarHandleProbe.class, "num", int.class);
        } catch (Exception e) { throw new ExceptionInInitializerError(e); }
    }

    static void report(String what, long ops, long ns) {
        System.out.printf("@@VH %-28s ops=%d ns_per_op=%.1f total_ms=%d%n",
                what, ops, (double) ns / ops, ns / 1_000_000);
    }

    public static void main(String[] args) {
        int iters = Integer.getInteger("probe.iters", 20_000_000);
        HibfixVarHandleProbe p = new HibfixVarHandleProbe();
        Node a = new Node(), b = new Node();
        AtomicReference<Node> ar = new AtomicReference<>(a);
        AtomicInteger ai = new AtomicInteger();

        // warm every path before timing any of it
        for (int i = 0; i < 200_000; i++) {
            REF.compareAndSet(p, p.ref, a); REF.set(p, b); p.plain = a;
            NUM.compareAndSet(p, p.num, i); ar.compareAndSet(ar.get(), b); ai.incrementAndGet();
        }

        long t;
        t = System.nanoTime();
        for (int i = 0; i < iters; i++) { Node cur = (Node) REF.get(p); REF.compareAndSet(p, cur, cur == a ? b : a); }
        report("VarHandle.CAS reference", iters, System.nanoTime() - t);

        t = System.nanoTime();
        for (int i = 0; i < iters; i++) { REF.set(p, (i & 1) == 0 ? a : b); }
        report("VarHandle.set reference", iters, System.nanoTime() - t);

        t = System.nanoTime();
        for (int i = 0; i < iters; i++) { int cur = (int) NUM.get(p); NUM.compareAndSet(p, cur, cur + 1); }
        report("VarHandle.CAS int", iters, System.nanoTime() - t);

        t = System.nanoTime();
        for (int i = 0; i < iters; i++) { Node cur = ar.get(); ar.compareAndSet(cur, cur == a ? b : a); }
        report("AtomicReference.CAS", iters, System.nanoTime() - t);

        t = System.nanoTime();
        for (int i = 0; i < iters; i++) { ai.incrementAndGet(); }
        report("AtomicInteger.increment", iters, System.nanoTime() - t);

        t = System.nanoTime();
        for (int i = 0; i < iters; i++) { p.plain = (i & 1) == 0 ? a : b; }
        report("plain field store (baseline)", iters, System.nanoTime() - t);

        System.out.println("@@VH done ref=" + (p.ref == a) + " num=" + p.num + " ai=" + ai.get());
    }
}
