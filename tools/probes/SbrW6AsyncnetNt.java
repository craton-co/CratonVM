// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.concurrent.atomic.AtomicReference;

/**
 * SBR wave 6, lane {@code asyncnet}: the two reviewed {@code Intrinsic}s over
 * {@code sun.nio.ch.NativeThread} ({@code current()J}, {@code signal(J)V}),
 * diffed against HotSpot 25. Needs {@code --add-opens java.base/sun.nio.ch=ALL-UNNAMED}
 * on every arm.
 *
 * <pre>
 *   java --add-opens java.base/sun.nio.ch=ALL-UNNAMED SbrW6AsyncnetNt [iters]
 * </pre>
 */
public class SbrW6AsyncnetNt {
    static Method cur, sig, isNative, isVirtual;

    static String call(Method m, Object... a) {
        try {
            return String.valueOf(m.invoke(null, a));
        } catch (InvocationTargetException e) {
            Throwable t = e.getCause();
            return "ex:" + t.getClass().getName() + ":" + t.getMessage();
        } catch (Exception e) {
            return "!" + e;
        }
    }

    static String probe() throws Exception {
        long here = (Long) cur.invoke(null);
        long again = (Long) cur.invoke(null);
        AtomicReference<Long> other = new AtomicReference<>();
        Thread t = new Thread(() -> {
            try {
                other.set((Long) cur.invoke(null));
            } catch (Exception e) {
                other.set(Long.MIN_VALUE);
            }
        });
        t.start();
        t.join();
        AtomicReference<Long> virt = new AtomicReference<>();
        Thread v = Thread.ofVirtual().start(() -> {
            try {
                virt.set((Long) cur.invoke(null));
            } catch (Exception e) {
                virt.set(Long.MIN_VALUE);
            }
        });
        v.join();
        return "platformValid=" + (here != 0 && here != -1) + " stable=" + (here == again)
                + " perThread=" + (here != other.get()) + " otherValid=" + (other.get() != 0 && other.get() != -1)
                + " virtual=" + virt.get()
                + " sig0=" + call(sig, 0L) + " sigM1=" + call(sig, -1L) + " sigHere=" + call(sig, here)
                + " isNative(here)=" + call(isNative, here) + " isNative(-1)=" + call(isNative, -1L)
                + " isNative(0)=" + call(isNative, 0L)
                + " isVirtual(-1)=" + call(isVirtual, -1L) + " isVirtual(here)=" + call(isVirtual, here);
    }

    public static void main(String[] args) throws Exception {
        Class<?> nt = Class.forName("sun.nio.ch.NativeThread");
        cur = nt.getDeclaredMethod("current");
        sig = nt.getDeclaredMethod("signal", long.class);
        isNative = nt.getDeclaredMethod("isNativeThread", long.class);
        isVirtual = nt.getDeclaredMethod("isVirtualThread", long.class);
        cur.setAccessible(true);
        sig.setAccessible(true);
        isNative.setAccessible(true);
        isVirtual.setAccessible(true);
        String first = probe();
        System.out.println("row nt = " + first);
        int drift = 0;
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 200;
        for (int i = 0; i < iters; i++) {
            String v = probe();
            if (!v.equals(first)) {
                if (drift < 5) System.out.println("DRIFT " + v);
                drift++;
            }
        }
        System.out.println("rows 1 drift " + drift);
    }
}
