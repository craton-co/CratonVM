// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// j27 wave 2 (lane VHCOMP): static-field constant-VarHandle accesses in HOT,
// compiled methods (CRATONVM_JIT_VH_CONST_STATIC: a locked instruction on the
// static's payload instead of the natives' call). Page
// docs/feature-designs/native-dispatch-architecture.md.
// Every fact is deterministic and must equal HotSpot 25's; the timing rows print
// only with -Dj27.time=true.
//   java J27VhStaticCompiled
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

public class J27VhStaticCompiled {
    static int SI;
    static long SL;
    static int SB;
    static final int FINAL_SI = 9;
    static class Lazy { static int LI = 40; }
    static final VarHandle VH_SI, VH_SL, VH_SB, VH_FINAL, VH_LAZY;
    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            VH_SI = l.findStaticVarHandle(J27VhStaticCompiled.class, "SI", int.class);
            VH_SL = l.findStaticVarHandle(J27VhStaticCompiled.class, "SL", long.class);
            VH_SB = l.findStaticVarHandle(J27VhStaticCompiled.class, "SB", int.class);
            VH_FINAL = l.findStaticVarHandle(J27VhStaticCompiled.class, "FINAL_SI", int.class);
            VH_LAZY = l.findStaticVarHandle(Lazy.class, "LI", int.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static int xaddLoop(int n) {
        int last = 0;
        for (int i = 0; i < n; i++) last = (int) VH_SI.getAndAdd(1);
        return last;
    }

    static void xaddVoidLoop(int n) {
        for (int i = 0; i < n; i++) VH_SI.getAndAdd(1);
    }

    static int casIncrement(int n) {
        int fails = 0;
        for (int i = 0; i < n; i++) {
            while (true) {
                int v = (int) VH_SI.getVolatile();
                if (VH_SI.compareAndSet(v, v + 1)) break;
                fails++;
            }
        }
        return fails;
    }

    static long longLoop(int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) sum += (long) VH_SL.getAndAdd(3L);
        return sum;
    }

    static int bitsLoop(int n) {
        int acc = 0;
        for (int i = 0; i < n; i++) {
            acc ^= (int) VH_SB.getAndBitwiseOr(1 << (i & 15));
            acc ^= (int) VH_SB.getAndBitwiseAnd(~(1 << ((i + 3) & 15)));
            acc ^= (int) VH_SB.getAndBitwiseXor(i);
            acc += (int) VH_SB.getAndSet(acc & 0xFFFF);
            acc += (int) VH_SB.compareAndExchange(acc & 0xFFFF, i);
        }
        return acc;
    }

    static int mixedLoop(int n) {
        for (int i = 0; i < n; i++) {
            SI++;
            VH_SI.getAndAdd(2);
            SI -= 1;
        }
        return SI;
    }

    static int lazyLoop(int n) {
        for (int i = 0; i < n; i++) VH_LAZY.getAndAdd(1);
        return (int) VH_LAZY.get();
    }

    static String wrongSiteType() {
        try {
            long r = (long) VH_SI.getAndAdd(1L);
            return "returned_" + r;
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    static String finalWrite() {
        try {
            VH_FINAL.getAndAdd(1);
            return "returned";
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    public static void main(String[] args) throws Exception {
        SI = 0;
        int last = 0;
        for (int r = 0; r < 20; r++) last = xaddLoop(50_000);
        System.out.println("fact static-vh-hot-xadd " + SI + "/" + last);
        for (int r = 0; r < 20; r++) xaddVoidLoop(50_000);
        System.out.println("fact static-vh-hot-xadd-void " + SI);

        SI = 0;
        Thread[] ts = new Thread[4];
        for (int t = 0; t < ts.length; t++) {
            ts[t] = new Thread(() -> { for (int r = 0; r < 10; r++) xaddVoidLoop(50_000); });
            ts[t].start();
        }
        for (Thread t : ts) t.join();
        System.out.println("fact static-vh-threads-xadd " + SI);

        SI = 0;
        for (int t = 0; t < ts.length; t++) {
            ts[t] = new Thread(() -> { for (int r = 0; r < 10; r++) casIncrement(20_000); });
            ts[t].start();
        }
        for (Thread t : ts) t.join();
        System.out.println("fact static-vh-threads-cas " + SI);

        SL = 1L << 40;
        long sum = 0;
        for (int r = 0; r < 20; r++) sum += longLoop(10_000);
        System.out.println("fact static-vh-hot-long " + SL + "/" + sum);

        SB = 0;
        int acc = 0;
        for (int r = 0; r < 20; r++) acc += bitsLoop(10_000);
        System.out.println("fact static-vh-hot-bits " + SB + "/" + acc);

        SI = 0;
        for (int r = 0; r < 20; r++) mixedLoop(10_000);
        System.out.println("fact static-vh-hot-mixed " + SI);

        System.out.println("fact static-vh-lazy " + lazyLoop(1) + "/" + lazyLoop(200_000));

        String wrong = null, fin = null;
        for (int r = 0; r < 20_000; r++) { wrong = wrongSiteType(); fin = finalWrite(); }
        System.out.println("fact static-vh-wrong-site-type " + wrong);
        System.out.println("fact static-vh-final-write " + fin + "/" + FINAL_SI);

        if (Boolean.getBoolean("j27.time")) {
            for (int rep = 0; rep < 7; rep++) {
                int n = 5_000_000;
                long t0 = System.nanoTime();
                xaddVoidLoop(n);
                long t1 = System.nanoTime();
                System.out.println("time static-vh-int-xadd " + ((t1 - t0) / n) + " ns/op");
            }
        }
        System.out.println("DONE");
    }
}
