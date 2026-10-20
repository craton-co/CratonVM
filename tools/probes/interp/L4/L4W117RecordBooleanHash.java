// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 117, lane L4 (page
// docs/internal/fixed-bugs/interpreter-i39-L4-proposal-one-record-layout-table-per-class-KEPT-OFF-20261224.md):
// the record intrinsic's `boolean`-component memo moved from a process-wide
// `static` to per-VM state. `boolean` components hash as `Boolean.hashCode`
// (1231 / 1237), not as the int 1 / 0, and an `int` component holding 0 / 1
// hashes as itself, for many record classes and many calls (the memo's hit
// and miss paths). Every row must be HotSpot's, both modes, JIT and --nojit.
//
// Run: javac -d . L4W117RecordBooleanHash.java
//      java -cp . L4W117RecordBooleanHash                 (HotSpot)
//      cratonvm --jdk-only|--compatible [--nojit] -cp . L4W117RecordBooleanHash
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// Z false 1237
// Z true 1231
// I 0 0
// I 1 1
// ZI 38162 38347
// IZ 1262 1237
// ZZZ 1222569
// Mixed 147538394
// equals true false
// toString IZ[i=1, b=true]
// warm-sum 25314438078
// HOTSPOT_EXPECTED_END
public class L4W117RecordBooleanHash {
    record Z(boolean b) {}
    record I(int i) {}
    record ZI(boolean b, int i) {}
    record IZ(int i, boolean b) {}
    record ZZZ(boolean a, boolean b, boolean c) {}
    record Mixed(String s, boolean b, long l, boolean c, int i) {}

    public static void main(String[] args) {
        System.out.println("Z false " + new Z(false).hashCode());
        System.out.println("Z true " + new Z(true).hashCode());
        System.out.println("I 0 " + new I(0).hashCode());
        System.out.println("I 1 " + new I(1).hashCode());
        System.out.println("ZI " + new ZI(true, 1).hashCode() + " " + new ZI(false, 0).hashCode());
        System.out.println("IZ " + new IZ(1, true).hashCode() + " " + new IZ(0, false).hashCode());
        System.out.println("ZZZ " + new ZZZ(true, false, true).hashCode());
        System.out.println("Mixed " + new Mixed("x", true, 5L, false, 1).hashCode());
        System.out.println("equals " + new ZI(true, 1).equals(new ZI(true, 1)) + " "
                + new ZI(true, 1).equals(new ZI(false, 1)));
        System.out.println("toString " + new IZ(1, true));
        long sum = 0;
        for (int k = 0; k < 20000; k++) {
            sum += new Z((k & 1) == 0).hashCode() + new I(k & 1).hashCode()
                    + new ZI((k & 2) == 0, k & 1).hashCode() + new ZZZ(k % 3 == 0, true, false).hashCode();
        }
        System.out.println("warm-sum " + sum);
    }
}
