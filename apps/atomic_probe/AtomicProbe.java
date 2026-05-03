import java.util.concurrent.atomic.*;
import java.util.concurrent.*;
public class AtomicProbe {
    public static void main(String[] a) throws Exception {
        // Test 1: AtomicInteger CAS sequential
        AtomicInteger ai = new AtomicInteger(0);
        boolean cas1 = ai.compareAndSet(0, 1);
        boolean cas2 = ai.compareAndSet(0, 2);
        System.out.println("ai.cas1=" + cas1 + " cas2=" + cas2 + " val=" + ai.get());  // true false 1

        // Test 2: AtomicLong CAS sequential
        AtomicLong al = new AtomicLong(0L);
        boolean lcas = al.compareAndSet(0L, 100L);
        System.out.println("al.cas=" + lcas + " val=" + al.get());  // true 100

        // Test 3: AtomicReference reference-identity CAS
        String s1 = "hello";
        String s2 = "world";
        AtomicReference<String> ar = new AtomicReference<>(s1);
        boolean rcas1 = ar.compareAndSet(s1, s2);  // true (identity)
        boolean rcas2 = ar.compareAndSet(new String("hello"), s1);  // false (different identity)
        System.out.println("ar.rcas1=" + rcas1 + " rcas2=" + rcas2 + " val=" + ar.get());

        // Test 4: 8-thread x 100k AtomicInteger contention
        AtomicInteger contended = new AtomicInteger(0);
        int N = 8, ITER = 100_000;
        Thread[] ts = new Thread[N];
        for (int i = 0; i < N; i++) {
            ts[i] = new Thread(() -> {
                for (int j = 0; j < ITER; j++) {
                    int old;
                    do { old = contended.get(); } while (!contended.compareAndSet(old, old + 1));
                }
            });
            ts[i].start();
        }
        for (Thread t : ts) t.join();
        System.out.println("contended.final=" + contended.get() + " expected=" + (long)N * ITER);

        System.out.println("OK");
    }
}
