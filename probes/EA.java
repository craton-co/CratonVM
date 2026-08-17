import java.util.concurrent.atomic.AtomicInteger;

public class EA {
    static final AtomicInteger ATOMIC = new AtomicInteger();
    static int plain;
    static class WithAtomic { final int id; WithAtomic() { id = ATOMIC.getAndIncrement(); } }
    static class WithPlain  { final int id; WithPlain()  { id = ++plain; } }
    static class Empty      { Empty() { } }

    static void loopAtomic(int n) { for (int i = 0; i < n; i++) { new WithAtomic(); } }
    static void loopPlain(int n)  { for (int i = 0; i < n; i++) { new WithPlain(); } }
    static void loopEmpty(int n)  { for (int i = 0; i < n; i++) { new Empty(); } }

    public static void main(String[] a) {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 1000000;
        long t0 = System.nanoTime();
        loopAtomic(n);
        loopPlain(n);
        long emptyStart = System.nanoTime();
        loopEmpty(n);
        long t1 = System.nanoTime();
        System.out.println("atomic ctor = " + ATOMIC.get());
        System.out.println("plain  ctor = " + plain);
        System.out.println("expected    = " + n);
        System.out.println("atomicOK    = " + (ATOMIC.get() == n));
        System.out.println("plainOK     = " + (plain == n));
        System.out.println("emptyCtorNs = " + ((t1 - emptyStart) / Math.max(1, n)));
        System.out.println("totalMs     = " + ((t1 - t0) / 1_000_000));
    }
}
