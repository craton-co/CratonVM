// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// j27 wave 2 (lane INVOKE): page
// docs/feature-designs/native-dispatch-architecture.md (bind-each / bind-only).
// Checks what CRATONVM_JIT_MH_BIND_COUNT_RAW (the compiled bind's raw `checkCustomized`
// count) and CRATONVM_MH_BINDTO_REAL_MINT_BATCH (the batched `bindTo` memo mint) must keep:
// every fresh bound handle called once counts 1; a non-constant handle is customized on
// its 128th call, count 127; the sums of the two rows.
// Run: java --add-opens java.base/java.lang.invoke=ALL-UNNAMED J27InvokeBindCount
// Facts are deterministic (the handles are never compile constants); timing lines only
// with -Dj27.time=true.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Field;

public class J27InvokeBindCount {
    static class Acc {
        final int base;

        Acc(int base) {
            this.base = base;
        }

        int plus(int x) {
            return base + x;
        }
    }

    static final MethodHandle PLUS;
    static final int N = 200_000;
    static final MethodHandle[] SAMPLES = new MethodHandle[N / 1024 + 1];
    static Field COUNT;
    static Field FORM;
    static Field CUSTOMIZED;

    static {
        try {
            PLUS = MethodHandles.lookup()
                    .findVirtual(Acc.class, "plus", MethodType.methodType(int.class, int.class));
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static int call(MethodHandle h, int x) throws Throwable {
        return (int) h.invokeExact(x);
    }

    static long bindEach(int n, boolean sample) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) {
            MethodHandle h = PLUS.bindTo(new Acc(i & 15));
            s += call(h, i);
            if (sample && (i & 1023) == 0) {
                SAMPLES[i >> 10] = h;
            }
        }
        return s;
    }

    static long bindOnly(int n) {
        long s = 0;
        Acc a = new Acc(3);
        for (int i = 0; i < n; i++) {
            MethodHandle h = PLUS.bindTo(a);
            s += h.type().parameterCount();
        }
        return s;
    }

    static String state(MethodHandle h) throws ReflectiveOperationException {
        Object form = FORM.get(h);
        return COUNT.getByte(h) + " customized=" + (CUSTOMIZED.get(form) != null);
    }

    public static void main(String[] args) throws Throwable {
        boolean time = Boolean.getBoolean("j27.time");
        try {
            COUNT = MethodHandle.class.getDeclaredField("customizationCount");
            FORM = MethodHandle.class.getDeclaredField("form");
            CUSTOMIZED = Class.forName("java.lang.invoke.LambdaForm").getDeclaredField("customized");
            COUNT.setAccessible(true);
            FORM.setAccessible(true);
            CUSTOMIZED.setAccessible(true);
        } catch (RuntimeException | ReflectiveOperationException e) {
            System.out.println("fact reflect-blocked " + e.getClass().getName());
            return;
        }
        // Warm `call` and `bindEach` with fresh handles, then keep the last pass's samples.
        for (int w = 0; w < 5; w++) {
            bindEach(N, false);
        }
        System.out.println("fact bind-each-sum " + bindEach(N, true));
        int ones = 0;
        String other = "none";
        for (MethodHandle h : SAMPLES) {
            if (h == null) {
                continue;
            }
            if (COUNT.getByte(h) == 1) {
                ones++;
            } else {
                other = state(h);
            }
        }
        System.out.println("fact fresh-called-once-count-1 " + ones + " other=" + other);
        // One fresh handle through the warm site, call by call.
        MethodHandle t = PLUS.bindTo(new Acc(7));
        long s = 0;
        for (int i = 0; i < 100; i++) {
            s += call(t, i);
        }
        System.out.println("fact after-100 " + state(t));
        for (int i = 0; i < 27; i++) {
            s += call(t, i);
        }
        System.out.println("fact after-127 " + state(t));
        s += call(t, 1);
        System.out.println("fact after-128 " + state(t));
        for (int i = 0; i < 1000; i++) {
            s += call(t, i);
        }
        System.out.println("fact after-1128 " + state(t) + " sum=" + s);
        for (int w = 0; w < 5; w++) {
            bindOnly(N);
        }
        System.out.println("fact bind-only-sum " + bindOnly(N));
        if (time) {
            long t0 = System.nanoTime();
            bindEach(N, false);
            long t1 = System.nanoTime();
            bindOnly(N);
            long t2 = System.nanoTime();
            System.out.println("time bind-each " + (t1 - t0) / N + " ns/op");
            System.out.println("time bind-only " + (t2 - t1) / N + " ns/op");
        }
    }
}
