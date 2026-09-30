// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Acceptance probe for admitting a protected `newarray` / `anewarray` / `multianewarray` as a
// precise-frame site (jit/src/lib.rs, `precise_alloc_athrow_enabled`). Every method below has an
// exception handler that reads a local written INSIDE the try, so the compiled body must publish
// that local through a reason-9 frame at every throwing site -- which for these three opcodes is a
// NegativeArraySizeException or (for anewarray) a class-resolution failure.
//
//   javac -d out Rbc6ArrayAllocProbe.java Gone.java && rm out/Gone.class   # Gone is compiled, then removed
//   java -cp out Rbc6ArrayAllocProbe 200000            (HotSpot: the oracle)
//   cratonvm [--jdk-only] [--nojit] -cp out Rbc6ArrayAllocProbe 200000
//   CRATONVM_JIT_NO_PRECISE_ALLOC_ATHROW=1 cratonvm ...     (the pre-change behaviour, same binary)
//
// Every line printed must be identical in all of those. `missingComponent` is the one that can
// genuinely raise at an anewarray for a reason other than length: `Gone.class` names a class whose
// file is not there, so the anewarray throws NoClassDefFoundError and the handler must still see `x`.
public class Rbc6ArrayAllocProbe {
    static final class Present {}

    static int newarrayPath(int a) {
        int x = a * 3;
        try {
            x += 1;
            int[] arr = new int[a];
            x += arr.length;
        } catch (NegativeArraySizeException e) {
            return x + e.getMessage().length();
        }
        return x;
    }

    static int anewarrayPath(int a) {
        int x = a + 7;
        try {
            x += 1;
            Present[] arr = new Present[a];
            x += arr.length;
        } catch (NegativeArraySizeException e) {
            return x * 2 + e.getMessage().length();
        }
        return x;
    }

    static int missingComponent(int a) {
        int x = a * 11;
        try {
            x += 5;
            Gone[] arr = new Gone[Math.abs(a) + 1];
            x += arr.length;
        } catch (NoClassDefFoundError e) {
            return x;
        }
        return -1;
    }

    // Outer dimension varies (and goes negative); inner is fixed and never negative, so the
    // expected message is unambiguous regardless of which dimension an implementation checks first.
    static int multianewarrayOuterPath(int a) {
        int x = a - 2;
        try {
            x += 1;
            int[][] arr = new int[a][3];
            x += arr.length;
        } catch (NegativeArraySizeException e) {
            return x + e.getMessage().length();
        }
        return x;
    }

    // Inner dimension varies (and goes negative); outer is fixed and never negative.
    static int multianewarrayInnerPath(int a) {
        int x = a - 5;
        try {
            x += 1;
            int[][] arr = new int[2][a];
            x += arr.length;
        } catch (NegativeArraySizeException e) {
            return x + e.getMessage().length();
        }
        return x;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;
        long s1 = 0, s2 = 0, s3 = 0, s4 = 0, s5 = 0;
        for (int i = -1000; i < n; i++) {
            s1 += newarrayPath(i);
            s2 += anewarrayPath(i);
            s3 += missingComponent(i);
            s4 += multianewarrayOuterPath(i);
            s5 += multianewarrayInnerPath(i);
        }
        System.out.println("newarrayPath         " + s1);
        System.out.println("anewarrayPath        " + s2);
        System.out.println("missingComponent     " + s3);
        System.out.println("multianewarrayOuter  " + s4);
        System.out.println("multianewarrayInner  " + s5);
    }
}
