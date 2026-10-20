// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;
import java.util.ArrayList;

/**
 * gch h2/fj (2026-10-08, {@code gch-h1jp-dispatch-doors-and-oome-ladder-review-findings}
 * item 4, {@code gcg-g4oo-oome-ladder-review-findings} item 4): the FIRST call
 * of a {@link Proxy} method whose signature names two classes nobody has
 * initialised, made on a heap filled to the last few hundred bytes. CratonVM's
 * proxy doors ({@code proxy_invoke_handler}, {@code proxy_invoke_handler_shared},
 * {@code vm/src/vm/vm_exec.rs}) build the call's {@code Method} on that first
 * call and mint its parameter types' class mirrors there; the mint used the
 * infallible arm, which aborts the process on Generational and ZGC. HotSpot
 * minted the mirrors at load and either runs the handler or throws a catchable
 * {@code OutOfMemoryError} (the proxy's argument array).
 *
 * <p>Each step frees {@code step} more small arrays of the fill, lets a failing
 * huge request collect them, and tries the call: an {@code OutOfMemoryError}
 * means "not yet", a result ends the ladder. Somewhere on it the {@code Method}
 * fits and the mirrors do not: the abort before the fix.
 *
 * <p>STDOUT (deterministic, identical on HotSpot 25):
 * <pre>
 *   proxy-mirror-oome PASS
 * </pre>
 * STDERR: {@code steps=N oome=K} (how far the ladder went).
 *
 * <pre>
 *   GchH2fjProxyMirrorOomeProbe [maxSteps=400]
 * </pre>
 * Run with {@code -Xmx32m}.
 */
public final class GchH2fjProxyMirrorOomeProbe {
    /** Never initialised: no static method, no field read, no instance. */
    static final class NeverA {
        static int touched = 1;
    }

    static final class NeverB {
        static int touched = 2;
    }

    interface Svc {
        Object call(NeverA a, NeverB b);
    }

    static final class Handler implements InvocationHandler {
        @Override
        public Object invoke(Object proxy, Method m, Object[] args) {
            return m.getName();
        }
    }

    public static void main(String[] argv) {
        int maxSteps = argv.length > 0 ? Integer.parseInt(argv[0]) : 400;
        Svc svc = (Svc) Proxy.newProxyInstance(
                GchH2fjProxyMirrorOomeProbe.class.getClassLoader(),
                new Class<?>[] {Svc.class},
                new Handler());
        // A legal request bigger than the whole heap: it collects, then fails.
        int hugeLen = (int) Math.min(Integer.MAX_VALUE - 16L, Runtime.getRuntime().maxMemory() / 8 + 1024);
        ArrayList<long[]> hog = new ArrayList<>(1 << 20);
        String result = null;
        int steps = 0;
        int oome = 0;
        try {
            try {
                while (true) {
                    hog.add(new long[1 << 12]);
                }
            } catch (OutOfMemoryError e) {
                // the coarse fill is done
            }
            try {
                while (true) {
                    hog.add(new long[16]);
                }
            } catch (OutOfMemoryError e) {
                // the fine fill is done
            }
            for (steps = 1; steps <= maxSteps && result == null; steps++) {
                for (int k = 0; k < steps && !hog.isEmpty(); k++) {
                    hog.remove(hog.size() - 1);
                }
                try {
                    long[] huge = new long[hugeLen];
                    huge[0] = 1;
                } catch (OutOfMemoryError e) {
                    // the failing request collected what the step freed
                }
                try {
                    result = String.valueOf(svc.call(null, null));
                } catch (OutOfMemoryError e) {
                    oome++;
                }
            }
        } finally {
            hog = null;
        }
        System.gc();
        System.err.println("steps=" + (steps - 1) + " oome=" + oome);
        boolean ok = result == null || "call".equals(result);
        System.out.println("proxy-mirror-oome " + (ok ? "PASS" : "FAIL " + result));
    }
}
