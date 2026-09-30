// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Acceptance probe for admitting a protected `ldc` / `ldc_w` as a precise-frame site (jit/src/lib.rs,
// `precise_alloc_athrow_enabled`). Every method below has an exception handler that reads a local written
// INSIDE the try, so the compiled body must publish that local through a reason-9 frame at every throwing
// site -- which for an ldc is a class-resolution failure or an interning allocation failure.
//
//   javac -d out Rbc6LdcProbe.java Gone.java && rm out/Gone.class     # Gone is compiled, then removed
//   java -cp out Rbc6LdcProbe 200000            (HotSpot: the oracle)
//   cratonvm [--jdk-only] [--nojit] -cp out Rbc6LdcProbe 200000
//   CRATONVM_JIT_NO_PRECISE_ALLOC_ATHROW=1 cratonvm ...     (the pre-change behaviour, same binary)
//
// Every line printed must be identical in all of those. `missingClass` is the one that can genuinely raise at
// an ldc: `Gone.class` names a class whose file is not there, so the ldc throws NoClassDefFoundError and the
// handler must still see `x`.
public class Rbc6LdcProbe {
    static final class Present {}

    static int stringPath(int a) {
        int x = a * 3;
        try {
            x += 1;
            String s = "constant";
            x += s.length();
            if (a < 0) {
                throw new IllegalStateException("negative");
            }
            x += 2;
        } catch (IllegalStateException e) {
            return x + e.getMessage().length();
        }
        return x;
    }

    static int classPath(int a) {
        int x = a + 7;
        try {
            Class<?> c = Present.class;
            x += c.getSimpleName().length();
            if ((a & 3) == 0) {
                throw new IllegalArgumentException("four");
            }
        } catch (IllegalArgumentException e) {
            return x * 2;
        }
        return x;
    }

    static int missingClass(int a) {
        int x = a * 11;
        try {
            x += 5;
            Class<?> c = Gone.class;
            x += c.hashCode();
        } catch (NoClassDefFoundError e) {
            return x;
        }
        return -1;
    }

    // ConcurrentHashMap.putVal's shape: a monitor handler (which reads the copy of the lock) around a
    // `throw new X("msg")`.
    static int syncShape(Object lock, int a) {
        int x = a;
        synchronized (lock) {
            x += 1;
            if (a == Integer.MIN_VALUE) {
                throw new IllegalStateException("Recursive update");
            }
            x += 2;
        }
        return x;
    }

    static int syncThrows(Object lock, int a) {
        int x = a;
        try {
            synchronized (lock) {
                x += 1;
                if ((a & 1023) == 0) {
                    throw new IllegalStateException("Recursive update");
                }
                x += 2;
            }
        } catch (IllegalStateException e) {
            return x * 7 + e.getMessage().length();
        }
        return x;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;
        Object lock = new Object();
        long s1 = 0, s2 = 0, s3 = 0, s4 = 0, s5 = 0;
        for (int i = -1000; i < n; i++) {
            s1 += stringPath(i);
            s2 += classPath(i);
            s3 += missingClass(i);
            s4 += syncShape(lock, i);
            s5 += syncThrows(lock, i);
        }
        System.out.println("stringPath   " + s1);
        System.out.println("classPath    " + s2);
        System.out.println("missingClass " + s3);
        System.out.println("syncShape    " + s4);
        System.out.println("syncThrows   " + s5);
    }
}
