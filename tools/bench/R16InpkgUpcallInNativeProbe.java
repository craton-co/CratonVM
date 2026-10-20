// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.SymbolLookup;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

/**
 * JIT round 16 wave 12 (lane inpkg): an FFM up-call that C code makes from
 * inside a JNI native must run its Java as a mutator the collector waits for
 * and scans
 * ({@code docs/internal/fixed-bugs/r16-ffm-takeover-signal-interrupts-blocking-syscalls-in-native-FIXED-20261005.md},
 * wave 12 section, doors D1/D2).
 *
 * <p>Two cases, each {@link #ROUNDS} calls from a compiled method while a
 * churner thread allocates (so the VM collects during the up-calls' Java):
 * <ul>
 *   <li>{@code upcall-in-jni}: the JNI native {@code callBack} calls an FFM
 *       up-call stub {@link #N} times; the stub's Java ({@link #cb})
 *       allocates and keeps an object across further allocation.</li>
 *   <li>{@code jni-in-upcall-in-ffm}: an FFM downcall to {@code ffmEntry}
 *       calls an up-call stub ({@link #outer}) whose Java calls
 *       {@code callBack} -- a JNI native inside an FFM downcall, whose C code
 *       calls the inner stub.</li>
 * </ul>
 * Each prints {@code <case>: ok} when every sum matched (the sums are
 * deterministic), and {@code PASS} at the end; HotSpot prints the same.
 * Elapsed time goes to stderr only.
 *
 * <p>Commands (Linux):
 * <pre>
 *   gcc -O1 -shared -fPIC -I"$JDK/include" -I"$JDK/include/linux" \
 *       -o /tmp/libr16inpkgupcall.so tools/probes/jni/R16InpkgUpcallInNativeProbe.c
 *   javac -d /tmp/r16inpkgupcall tools/bench/R16InpkgUpcallInNativeProbe.java
 *   java --enable-native-access=ALL-UNNAMED -Xmx64m -cp /tmp/r16inpkgupcall \
 *       R16InpkgUpcallInNativeProbe /tmp/libr16inpkgupcall.so
 *   # package arm (the doors fix is on with it):
 *   CRATONVM_JNI_NATIVE_TRANSITIONS=1 cratonvm --java-home $JDK -Xmx64m \
 *       -cp /tmp/r16inpkgupcall R16InpkgUpcallInNativeProbe /tmp/libr16inpkgupcall.so
 *   # positive control (Generational): both arms plus
 *   #   CRATONVM_DBG_BLOCKED_ACCESS=warn
 *   # the fixed arm prints no "[blocked-access]" line; with
 *   #   CRATONVM_JNI_IN_NATIVE_DOORS=0
 *   # the up-call's Java runs with the blocked flag up and it does.
 * </pre>
 */
public final class R16InpkgUpcallInNativeProbe {

    static final int ROUNDS = 400;
    static final int N = 50;
    static final int WARM = 2_000;

    static native long callBack(long fn, int n);

    static volatile boolean done;
    static volatile long sink;
    static volatile Object keep;

    static long innerStub;
    static MethodHandle ffmEntry;
    static MemorySegment outerStub;

    /** The inner up-call target: allocates, then reads its object back. */
    static long cb(int i) {
        long[] a = new long[64];
        a[i & 63] = i;
        String s = "x" + i;
        keep = new byte[256];
        return a[i & 63] + s.length();
    }

    /** The outer up-call target: calls the JNI native from Java. */
    static long outer(int n) {
        return callBack(innerStub, n);
    }

    static long expected(int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += i + ("x" + i).length();
        }
        return sum;
    }

    /** The compiled caller of case 1. */
    static long viaJni(int n) {
        return callBack(innerStub, n);
    }

    /** The compiled caller of case 2. */
    static long viaFfm(int n) throws Throwable {
        return (long) ffmEntry.invokeExact(outerStub, n);
    }

    static long churn(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            byte[] b = new byte[4096];
            b[i & 4095] = (byte) i;
            acc += b[(i * 7) & 4095];
        }
        return acc;
    }

    public static void main(String[] args) throws Throwable {
        System.load(args[0]);
        Linker linker = Linker.nativeLinker();
        Arena arena = Arena.global();
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodHandle cbHandle = lookup.findStatic(R16InpkgUpcallInNativeProbe.class, "cb",
                MethodType.methodType(long.class, int.class));
        MethodHandle outerHandle = lookup.findStatic(R16InpkgUpcallInNativeProbe.class, "outer",
                MethodType.methodType(long.class, int.class));
        FunctionDescriptor longOfInt = FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.JAVA_INT);
        innerStub = linker.upcallStub(cbHandle, longOfInt, arena).address();
        outerStub = linker.upcallStub(outerHandle, longOfInt, arena);
        SymbolLookup lib = SymbolLookup.libraryLookup(args[0], arena);
        ffmEntry = linker.downcallHandle(lib.find("ffmEntry").orElseThrow(),
                FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.ADDRESS, ValueLayout.JAVA_INT));

        long want = expected(N);
        for (int i = 0; i < WARM; i++) {
            sink += viaJni(2) + viaFfm(2);
        }
        Thread churner = new Thread(() -> {
            while (!done) {
                sink += churn(10_000);
            }
        }, "churner");
        long t0 = System.nanoTime();
        churner.start();
        int[] bad = new int[2];
        for (int r = 0; r < ROUNDS; r++) {
            if (viaJni(N) != want) {
                bad[0]++;
            }
            if (viaFfm(N) != want) {
                bad[1]++;
            }
        }
        done = true;
        churner.join(120_000);
        System.err.println("elapsed " + (System.nanoTime() - t0) / 1_000_000 + " ms");
        String[] names = {"upcall-in-jni", "jni-in-upcall-in-ffm"};
        boolean ok = true;
        for (int c = 0; c < 2; c++) {
            if (bad[c] == 0) {
                System.out.println(names[c] + ": ok");
            } else {
                ok = false;
                System.out.println(names[c] + ": " + bad[c] + " wrong sums of " + ROUNDS);
            }
        }
        System.out.println(ok ? "PASS" : "FAIL");
    }
}
