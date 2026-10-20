/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 */

/**
 * gci i8/dm (2026-10-13): soundness of the per-thread JNI dispatch memo
 * ({@code CRATONVM_JNI_DISPATCH_MEMO}). Every case is a loop long enough to
 * reach compiled code, so the memo's hit serves most calls; stdout is
 * HotSpot's exact answers (compared byte for byte), diagnostics go to stderr.
 *
 * <p>Usage: {@code GciDm8JniMemoProbe /abs/path/gcidm8jnimemo.dll [n]}.
 * Native half: {@code tools/probes/jni/GciDm8JniMemoProbe.c}.
 */
public class GciDm8JniMemoProbe {
    static native int add1(int x);

    static synchronized native boolean syncHeld();

    static synchronized native void syncBump();

    static native int bind(int which);

    static native int swapped(int x);

    static native int throwIf(int x);

    static int counter;

    static boolean held() {
        return Thread.holdsLock(GciDm8JniMemoProbe.class);
    }

    static void bump() {
        int c = counter;
        for (int i = 0; i < 20; i++) {
            if ((c & 1) == 2) {
                c++;
            }
        }
        counter = c + 1;
    }

    static class Base {
        native int instAdd(int x);

        synchronized native boolean instHeld();

        boolean heldSelf() {
            return Thread.holdsLock(this);
        }
    }

    static class Sub extends Base {
    }

    static class Over extends Base {
        @Override
        int instAdd(int x) {
            return x - 1;
        }
    }

    abstract static class Abs {
        native int absGet(int x);

        abstract int other();
    }

    static final class Conc extends Abs {
        @Override
        int other() {
            return 1;
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length < 1) {
            System.out.println("usage: GciDm8JniMemoProbe /abs/path/gcidm8jnimemo.dll [n]");
            return;
        }
        System.load(args[0]);
        final int n = args.length > 1 ? Integer.parseInt(args[1]) : 200000;

        long s = 0;
        for (int i = 0; i < n; i++) {
            s += add1(i);
        }
        System.out.println("static: " + s);

        boolean allHeld = true;
        for (int i = 0; i < 20000; i++) {
            allHeld &= syncHeld();
        }
        System.out.println("sync-static-held: " + allHeld);

        final int threads = 4;
        final int each = 5000;
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            ts[t] = new Thread(() -> {
                for (int i = 0; i < each; i++) {
                    syncBump();
                }
            });
            ts[t].start();
        }
        for (Thread t : ts) {
            t.join();
        }
        System.out.println("sync-static-count: " + counter);

        Base b = new Sub();
        boolean instHeldAll = true;
        for (int i = 0; i < 20000; i++) {
            instHeldAll &= b.instHeld();
        }
        System.out.println("sync-instance-held: " + instHeldAll);

        s = 0;
        System.out.println("bind-a: " + bind(0));
        for (int i = 0; i < n; i++) {
            s += swapped(i);
        }
        System.out.println("bind-b: " + bind(1));
        for (int i = 0; i < n; i++) {
            s += swapped(i);
        }
        System.out.println("bind-a-again: " + bind(0));
        for (int i = 0; i < n; i++) {
            s += swapped(i);
        }
        System.out.println("rebind-sum: " + s);

        int caught = 0;
        s = 0;
        for (int i = 0; i < n; i++) {
            try {
                s += throwIf(i % 16);
            } catch (IllegalStateException e) {
                if ("seven".equals(e.getMessage())) {
                    caught++;
                }
            }
        }
        System.out.println("thrown: " + caught + " sum: " + s);

        Base[] rs = {new Base(), new Sub(), new Over()};
        s = 0;
        for (int i = 0; i < n; i++) {
            s += rs[i % 3].instAdd(i);
        }
        System.out.println("virtual: " + s);

        Abs a = new Conc();
        s = 0;
        for (int i = 0; i < n; i++) {
            s += a.absGet(i) + a.other();
        }
        System.out.println("abstract: " + s);
        System.err.println("[gci-dm8] done n=" + n);
        System.out.println("done");
    }
}
