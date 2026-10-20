/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 */

/**
 * zr z2/r2: the deciding probe for
 * docs/internal/gc/zgc-zr1rl-raw-jni-locals-on-a-native-peer-SUPERSEDED-20261019.md.
 *
 * A JNI native keeps a jobject in a C local while the collector may move the
 * object, then reads a field through that local. Two shapes:
 *
 *   spin    the native spins for spinMs WITHOUT calling into the VM while a
 *           second Java thread allocates garbage and calls System.gc(). A
 *           thread inside a JNI native is a counted mutator in CratonVM (the
 *           native transitions are opt-in), so the pause should wait for it:
 *           no collection can run under the spin at all.
 *   upcall  the native calls back into Java (allocate + System.gc()) and then
 *           reads through the local it held across the call. With the default
 *           INDIRECT locals the C copy is a (frame, slot) handle into the table
 *           every slide remaps; with CRATONVM_JNI_INDIRECT_LOCALS=0 it is the
 *           raw address, and nothing in ZGC's relocation refusal reads the
 *           raw-locals ledger.
 *
 * Each holder is allocated on a sparse page (garbage before and after it) so
 * the selector picks its page and the slide moves it.
 *
 * Output (HotSpot prints exactly these two lines):
 *   spin ok N/N
 *   upcall ok N/N
 *
 * Build (Linux): gcc -O1 -Wall -shared -fPIC -I$J/include -I$J/include/linux
 *   -o jnilib/libzr2r2jniraw.so tools/probes/jni/Zr2R2JniRawLocal.c
 * Run:  java -Djava.library.path=jnilib -cp <classes> Zr2R2JniRawLocal [rounds]
 */
public class Zr2R2JniRawLocal {
    static final class Holder {
        int v;
        long pad0, pad1, pad2;

        Holder(int v) {
            this.v = v;
        }
    }

    /** 0 = the field read through the held local is right; nonzero otherwise. */
    static native int spinThenRead(Holder h, int expected, int spinMs);

    /** 0 = right; bit 0 = IsSameObject(local, global) false; bit 1 = wrong field;
     *  bit 2 = the up-call left an exception pending. */
    static native int upcallThenRead(Holder h, Runnable churn, int expected);

    static volatile Object sink;

    static void garbage(int n) {
        for (int i = 0; i < n; i++) {
            sink = new long[8];
        }
    }

    static Holder sparseHolder(int v) {
        garbage(20_000);
        Holder h = new Holder(v);
        garbage(20_000);
        return h;
    }

    public static void main(String[] args) throws Exception {
        System.loadLibrary("zr2r2jniraw");
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 40;
        int spinMs = args.length > 1 ? Integer.parseInt(args[1]) : 200;

        // ---- spin: a collector thread runs while the native spins -----------
        int spinOk = 0;
        for (int r = 0; r < rounds; r++) {
            Holder h = sparseHolder(0x5A5A0000 | r);
            Thread churn = new Thread(() -> {
                garbage(200_000);
                System.gc();
            });
            churn.start();
            int code = spinThenRead(h, 0x5A5A0000 | r, spinMs);
            churn.join();
            if (code == 0 && h.v == (0x5A5A0000 | r)) {
                spinOk++;
            }
        }
        System.out.println("spin ok " + spinOk + "/" + rounds);

        // ---- upcall: the native holds the local across an allocating call ----
        Runnable churn = () -> {
            garbage(200_000);
            System.gc();
        };
        int upOk = 0;
        for (int r = 0; r < rounds; r++) {
            Holder h = sparseHolder(0x3C3C0000 | r);
            int code = upcallThenRead(h, churn, 0x3C3C0000 | r);
            if (code == 0) {
                upOk++;
            }
        }
        System.out.println("upcall ok " + upOk + "/" + rounds);
    }
}
