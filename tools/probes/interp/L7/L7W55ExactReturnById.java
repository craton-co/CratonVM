// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 55, lane L7: `CRATONVM_MH_EXACT_RETURN_BY_ID`
// (proposal docs/internal/fixed-bugs/interpreter-L7-proposal-the-invokeexact-return-check-by-class-id-FIXED-20261019.md).
// The strict `invokeExact` return check answers a box of the expected wrapper
// by its class id instead of the class-manager lock and a name compare. Each
// primitive return kind runs once cold (r0) and then in a loop (r1, r2), at
// one site per kind, so every wrapper's id is recorded and then answered;
// the values must be HotSpot's.
//
// Positive control (`--nojit`, `CRATONVM_DBG_FIELD_SITE=1`, FINAL line):
// `mh-exact-return-by-id: 3200000` with `CRATONVM_MH_EXACT_RETURN_BY_ID=1`
// (every call but each kind's first, which records the id) and 0 with `=0`
// (measured on the lane's debug build; the r-lines identical in both).
//
// Expected (HotSpot 25.0.3, `java L7W55ExactReturnById`, and `-Xint`):
//   r0 7 9000000000 true x 2.5 3.25 -3 300
//   r1 1400000 1800000000000000 200000 24000000 500000.0 650000.0 -600000 60000000
//   r2 1400000 1800000000000000 200000 24000000 500000.0 650000.0 -600000 60000000
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L7W55ExactReturnById {
    static int i() { return 7; }
    static long j() { return 9_000_000_000L; }
    static boolean z() { return true; }
    static char c() { return 'x'; }
    static double d() { return 2.5; }
    static float f() { return 3.25f; }
    static byte b() { return (byte) -3; }
    static short s() { return (short) 300; }

    static final MethodHandle I, J, Z, C, D, F, B, S;

    static {
        try {
            I = filtered("i", int.class);
            J = filtered("j", long.class);
            Z = filtered("z", boolean.class);
            C = filtered("c", char.class);
            D = filtered("d", double.class);
            F = filtered("f", float.class);
            B = filtered("b", byte.class);
            S = filtered("s", short.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    // An adapter (`filterReturnValue` with an identity filter), not the
    // direct handle: CratonVM's direct and bound handles return the primitive
    // unboxed, and only an adapter's native hands back the box the strict
    // check reads (measured on the wave-55 lane build: the counter stays 0
    // for `findStatic` and `bindTo`, and counts for `filterReturnValue`,
    // `insertArguments`, `asType` and `constant`).
    static MethodHandle filtered(String name, Class<?> type) throws ReflectiveOperationException {
        MethodHandle direct = MethodHandles.lookup()
                .findStatic(L7W55ExactReturnById.class, name, MethodType.methodType(type));
        return MethodHandles.filterReturnValue(direct, MethodHandles.identity(type));
    }

    static String row(int n) throws Throwable {
        long si = 0, sj = 0, sz = 0, sc = 0, sb = 0, ss = 0;
        double sd = 0, sf = 0;
        for (int k = 0; k < n; k++) {
            si += (int) I.invokeExact();
            sj += (long) J.invokeExact();
            sz += ((boolean) Z.invokeExact()) ? 1 : 0;
            sc += (char) C.invokeExact();
            sd += (double) D.invokeExact();
            sf += (float) F.invokeExact();
            sb += (byte) B.invokeExact();
            ss += (short) S.invokeExact();
        }
        if (n == 1) {
            return si + " " + sj + " " + (sz == 1) + " " + (char) sc + " " + sd + " " + sf + " " + sb + " " + ss;
        }
        return si + " " + sj + " " + sz + " " + sc + " " + sd + " " + sf + " " + sb + " " + ss;
    }

    public static void main(String[] args) throws Throwable {
        System.out.println("r0 " + row(1));
        System.out.println("r1 " + row(200_000));
        System.out.println("r2 " + row(200_000));
    }
}
