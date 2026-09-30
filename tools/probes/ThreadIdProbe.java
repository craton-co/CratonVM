// Plain ThreadLocal / currentCarrierThread identity across threads (clean on both VMs; the sharing was the carrier map).
//
// Record: nonpassed-classbyclass-census-RESOLVED-20260923.md (D2)
// Run: cratonvm --java-home <jdk-25> -c <dir>[:<classpath>] ThreadIdProbe <threads>
// Compare with the same command on HotSpot (java -cp ...).
import java.lang.reflect.Method;
import java.util.*;
import java.util.concurrent.atomic.*;

public class ThreadIdProbe {
    static final ThreadLocal<Object> TL = ThreadLocal.withInitial(Object::new);
    static final ThreadLocal<Object> TL2 = new ThreadLocal<>() { @Override protected Object initialValue() { return new Object(); } };

    public static void main(String[] a) throws Exception {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 200;
        Method carrier = null;
        try {
            carrier = Thread.class.getDeclaredMethod("currentCarrierThread");
            carrier.setAccessible(true);
        } catch (Throwable t) {
            System.out.println("no carrier access: " + t);
        }
        final Method cm = carrier;
        AtomicInteger curMismatch = new AtomicInteger(), carMismatch = new AtomicInteger(), tlShared = new AtomicInteger(), tl2Shared = new AtomicInteger();
        Set<Object> seen = Collections.synchronizedSet(Collections.newSetFromMap(new IdentityHashMap<>()));
        Set<Object> seen2 = Collections.synchronizedSet(Collections.newSetFromMap(new IdentityHashMap<>()));
        List<Thread> ts = new ArrayList<>();
        Object mainTl = TL.get(), mainTl2 = TL2.get();
        seen.add(mainTl); seen2.add(mainTl2);
        for (int i = 0; i < n; i++) {
            Thread[] self = new Thread[1];
            Thread t = new Thread(() -> {
                for (int k = 0; k < 50; k++) {
                    if (Thread.currentThread() != self[0]) curMismatch.incrementAndGet();
                    if (cm != null) {
                        try { if (cm.invoke(null) != self[0]) carMismatch.incrementAndGet(); } catch (Throwable e) { carMismatch.incrementAndGet(); }
                    }
                    Object v = TL.get();
                    Object v2 = TL2.get();
                    if (k == 0) {
                        if (!seen.add(v)) tlShared.incrementAndGet();
                        if (!seen2.add(v2)) tl2Shared.incrementAndGet();
                    }
                }
            });
            self[0] = t;
            ts.add(t);
        }
        for (Thread t : ts) t.start();
        // main keeps using its own thread locals concurrently
        for (int k = 0; k < 200000; k++) { if (TL.get() != mainTl || TL2.get() != mainTl2) { tlShared.addAndGet(1000); break; } }
        for (Thread t : ts) t.join();
        System.out.println("threads=" + n + " currentThreadMismatch=" + curMismatch + " carrierMismatch=" + carMismatch
                + " tlShared=" + tlShared + " tl2Shared=" + tl2Shared);
    }
}
