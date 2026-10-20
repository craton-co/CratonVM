// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench -- Interpreter round i4 wave 84, lane L2. What an interpreted implicit
// NullPointerException costs when nobody reads its message. The VM builds the
// JEP 358 message EAGERLY at the throw (the whole-method operand-stack
// analysis, `helpful_npe::null_expr_at_depth`, one state per bci), so the cost
// grows with the trapping method's size; HotSpot computes it only in
// `NullPointerException.getMessage()` (`getExtendedNPEMessage`). The compiled
// path already defers it under --jdk-only (`CRATONVM_NPE_DEFERRED_MESSAGE`);
// the interpreter does not unless CRATONVM_INTERP_NPE_DEFERRED_MESSAGE=1
// (wave 85; run the --nojit rows with it on and off). Page:
// docs/internal/fixed-bugs/interpreter-i84-L2-proposal-defer-the-interpreters-npe-message-DONE-20261118.md.
//
// Rows (ns per throw+catch, median of 5 interleaved reps; NOT deterministic):
//   small   a 4-byte method (`n.v`)
//   large   a ~3 KB method (240 statements, 120 branches), trapping at its end
//   read-*  the same, reading getMessage() (the cost a lazy message keeps)
// A large/small ratio near 1 under -XX:-ShowCodeDetailsInExceptionMessages and
// well above 1 without it is the analysis' share.
//
// Run:
//   $J/javac -d out tools/probes/interp/L2/L2W84NpeMessageCostBench.java
//   $B --nojit --java-home $J/.. -cp out L2W84NpeMessageCostBench
//   $B --nojit -XX:-ShowCodeDetailsInExceptionMessages --java-home $J/.. -cp out L2W84NpeMessageCostBench
//   $J/java -Xint -cp out L2W84NpeMessageCostBench       (reference)
// The last line, `check <message>`, is deterministic:
//   check Cannot read field "v" because "<parameter1>" is null
// (and `check null` under -XX:-ShowCodeDetailsInExceptionMessages).
import java.util.Arrays;

public class L2W84NpeMessageCostBench {
    static final class Node {
        int v;
    }

    static int small(Node n) {
        return n.v;
    }

    static int large(Node n, int k) {
        int a = k;
        a = a * 31 + k + 0;
        if (a > 1000) { a -= 7; } else { a += 0; }
        a = a * 31 + k + 1;
        if (a > 1001) { a -= 8; } else { a += 1; }
        a = a * 31 + k + 2;
        if (a > 1002) { a -= 9; } else { a += 2; }
        a = a * 31 + k + 3;
        if (a > 1003) { a -= 10; } else { a += 0; }
        a = a * 31 + k + 4;
        if (a > 1004) { a -= 11; } else { a += 1; }
        a = a * 31 + k + 5;
        if (a > 1005) { a -= 7; } else { a += 2; }
        a = a * 31 + k + 6;
        if (a > 1006) { a -= 8; } else { a += 0; }
        a = a * 31 + k + 7;
        if (a > 1007) { a -= 9; } else { a += 1; }
        a = a * 31 + k + 8;
        if (a > 1008) { a -= 10; } else { a += 2; }
        a = a * 31 + k + 9;
        if (a > 1009) { a -= 11; } else { a += 0; }
        a = a * 31 + k + 10;
        if (a > 1010) { a -= 7; } else { a += 1; }
        a = a * 31 + k + 11;
        if (a > 1011) { a -= 8; } else { a += 2; }
        a = a * 31 + k + 12;
        if (a > 1012) { a -= 9; } else { a += 0; }
        a = a * 31 + k + 13;
        if (a > 1013) { a -= 10; } else { a += 1; }
        a = a * 31 + k + 14;
        if (a > 1014) { a -= 11; } else { a += 2; }
        a = a * 31 + k + 15;
        if (a > 1015) { a -= 7; } else { a += 0; }
        a = a * 31 + k + 16;
        if (a > 1016) { a -= 8; } else { a += 1; }
        a = a * 31 + k + 17;
        if (a > 1017) { a -= 9; } else { a += 2; }
        a = a * 31 + k + 18;
        if (a > 1018) { a -= 10; } else { a += 0; }
        a = a * 31 + k + 19;
        if (a > 1019) { a -= 11; } else { a += 1; }
        a = a * 31 + k + 20;
        if (a > 1020) { a -= 7; } else { a += 2; }
        a = a * 31 + k + 21;
        if (a > 1021) { a -= 8; } else { a += 0; }
        a = a * 31 + k + 22;
        if (a > 1022) { a -= 9; } else { a += 1; }
        a = a * 31 + k + 23;
        if (a > 1023) { a -= 10; } else { a += 2; }
        a = a * 31 + k + 24;
        if (a > 1024) { a -= 11; } else { a += 0; }
        a = a * 31 + k + 25;
        if (a > 1025) { a -= 7; } else { a += 1; }
        a = a * 31 + k + 26;
        if (a > 1026) { a -= 8; } else { a += 2; }
        a = a * 31 + k + 27;
        if (a > 1027) { a -= 9; } else { a += 0; }
        a = a * 31 + k + 28;
        if (a > 1028) { a -= 10; } else { a += 1; }
        a = a * 31 + k + 29;
        if (a > 1029) { a -= 11; } else { a += 2; }
        a = a * 31 + k + 30;
        if (a > 1030) { a -= 7; } else { a += 0; }
        a = a * 31 + k + 31;
        if (a > 1031) { a -= 8; } else { a += 1; }
        a = a * 31 + k + 32;
        if (a > 1032) { a -= 9; } else { a += 2; }
        a = a * 31 + k + 33;
        if (a > 1033) { a -= 10; } else { a += 0; }
        a = a * 31 + k + 34;
        if (a > 1034) { a -= 11; } else { a += 1; }
        a = a * 31 + k + 35;
        if (a > 1035) { a -= 7; } else { a += 2; }
        a = a * 31 + k + 36;
        if (a > 1036) { a -= 8; } else { a += 0; }
        a = a * 31 + k + 37;
        if (a > 1037) { a -= 9; } else { a += 1; }
        a = a * 31 + k + 38;
        if (a > 1038) { a -= 10; } else { a += 2; }
        a = a * 31 + k + 39;
        if (a > 1039) { a -= 11; } else { a += 0; }
        a = a * 31 + k + 40;
        if (a > 1040) { a -= 7; } else { a += 1; }
        a = a * 31 + k + 41;
        if (a > 1041) { a -= 8; } else { a += 2; }
        a = a * 31 + k + 42;
        if (a > 1042) { a -= 9; } else { a += 0; }
        a = a * 31 + k + 43;
        if (a > 1043) { a -= 10; } else { a += 1; }
        a = a * 31 + k + 44;
        if (a > 1044) { a -= 11; } else { a += 2; }
        a = a * 31 + k + 45;
        if (a > 1045) { a -= 7; } else { a += 0; }
        a = a * 31 + k + 46;
        if (a > 1046) { a -= 8; } else { a += 1; }
        a = a * 31 + k + 47;
        if (a > 1047) { a -= 9; } else { a += 2; }
        a = a * 31 + k + 48;
        if (a > 1048) { a -= 10; } else { a += 0; }
        a = a * 31 + k + 49;
        if (a > 1049) { a -= 11; } else { a += 1; }
        a = a * 31 + k + 50;
        if (a > 1050) { a -= 7; } else { a += 2; }
        a = a * 31 + k + 51;
        if (a > 1051) { a -= 8; } else { a += 0; }
        a = a * 31 + k + 52;
        if (a > 1052) { a -= 9; } else { a += 1; }
        a = a * 31 + k + 53;
        if (a > 1053) { a -= 10; } else { a += 2; }
        a = a * 31 + k + 54;
        if (a > 1054) { a -= 11; } else { a += 0; }
        a = a * 31 + k + 55;
        if (a > 1055) { a -= 7; } else { a += 1; }
        a = a * 31 + k + 56;
        if (a > 1056) { a -= 8; } else { a += 2; }
        a = a * 31 + k + 57;
        if (a > 1057) { a -= 9; } else { a += 0; }
        a = a * 31 + k + 58;
        if (a > 1058) { a -= 10; } else { a += 1; }
        a = a * 31 + k + 59;
        if (a > 1059) { a -= 11; } else { a += 2; }
        a = a * 31 + k + 60;
        if (a > 1060) { a -= 7; } else { a += 0; }
        a = a * 31 + k + 61;
        if (a > 1061) { a -= 8; } else { a += 1; }
        a = a * 31 + k + 62;
        if (a > 1062) { a -= 9; } else { a += 2; }
        a = a * 31 + k + 63;
        if (a > 1063) { a -= 10; } else { a += 0; }
        a = a * 31 + k + 64;
        if (a > 1064) { a -= 11; } else { a += 1; }
        a = a * 31 + k + 65;
        if (a > 1065) { a -= 7; } else { a += 2; }
        a = a * 31 + k + 66;
        if (a > 1066) { a -= 8; } else { a += 0; }
        a = a * 31 + k + 67;
        if (a > 1067) { a -= 9; } else { a += 1; }
        a = a * 31 + k + 68;
        if (a > 1068) { a -= 10; } else { a += 2; }
        a = a * 31 + k + 69;
        if (a > 1069) { a -= 11; } else { a += 0; }
        a = a * 31 + k + 70;
        if (a > 1070) { a -= 7; } else { a += 1; }
        a = a * 31 + k + 71;
        if (a > 1071) { a -= 8; } else { a += 2; }
        a = a * 31 + k + 72;
        if (a > 1072) { a -= 9; } else { a += 0; }
        a = a * 31 + k + 73;
        if (a > 1073) { a -= 10; } else { a += 1; }
        a = a * 31 + k + 74;
        if (a > 1074) { a -= 11; } else { a += 2; }
        a = a * 31 + k + 75;
        if (a > 1075) { a -= 7; } else { a += 0; }
        a = a * 31 + k + 76;
        if (a > 1076) { a -= 8; } else { a += 1; }
        a = a * 31 + k + 77;
        if (a > 1077) { a -= 9; } else { a += 2; }
        a = a * 31 + k + 78;
        if (a > 1078) { a -= 10; } else { a += 0; }
        a = a * 31 + k + 79;
        if (a > 1079) { a -= 11; } else { a += 1; }
        a = a * 31 + k + 80;
        if (a > 1080) { a -= 7; } else { a += 2; }
        a = a * 31 + k + 81;
        if (a > 1081) { a -= 8; } else { a += 0; }
        a = a * 31 + k + 82;
        if (a > 1082) { a -= 9; } else { a += 1; }
        a = a * 31 + k + 83;
        if (a > 1083) { a -= 10; } else { a += 2; }
        a = a * 31 + k + 84;
        if (a > 1084) { a -= 11; } else { a += 0; }
        a = a * 31 + k + 85;
        if (a > 1085) { a -= 7; } else { a += 1; }
        a = a * 31 + k + 86;
        if (a > 1086) { a -= 8; } else { a += 2; }
        a = a * 31 + k + 87;
        if (a > 1087) { a -= 9; } else { a += 0; }
        a = a * 31 + k + 88;
        if (a > 1088) { a -= 10; } else { a += 1; }
        a = a * 31 + k + 89;
        if (a > 1089) { a -= 11; } else { a += 2; }
        a = a * 31 + k + 90;
        if (a > 1090) { a -= 7; } else { a += 0; }
        a = a * 31 + k + 91;
        if (a > 1091) { a -= 8; } else { a += 1; }
        a = a * 31 + k + 92;
        if (a > 1092) { a -= 9; } else { a += 2; }
        a = a * 31 + k + 93;
        if (a > 1093) { a -= 10; } else { a += 0; }
        a = a * 31 + k + 94;
        if (a > 1094) { a -= 11; } else { a += 1; }
        a = a * 31 + k + 95;
        if (a > 1095) { a -= 7; } else { a += 2; }
        a = a * 31 + k + 96;
        if (a > 1096) { a -= 8; } else { a += 0; }
        a = a * 31 + k + 97;
        if (a > 1097) { a -= 9; } else { a += 1; }
        a = a * 31 + k + 98;
        if (a > 1098) { a -= 10; } else { a += 2; }
        a = a * 31 + k + 99;
        if (a > 1099) { a -= 11; } else { a += 0; }
        a = a * 31 + k + 100;
        if (a > 1100) { a -= 7; } else { a += 1; }
        a = a * 31 + k + 101;
        if (a > 1101) { a -= 8; } else { a += 2; }
        a = a * 31 + k + 102;
        if (a > 1102) { a -= 9; } else { a += 0; }
        a = a * 31 + k + 103;
        if (a > 1103) { a -= 10; } else { a += 1; }
        a = a * 31 + k + 104;
        if (a > 1104) { a -= 11; } else { a += 2; }
        a = a * 31 + k + 105;
        if (a > 1105) { a -= 7; } else { a += 0; }
        a = a * 31 + k + 106;
        if (a > 1106) { a -= 8; } else { a += 1; }
        a = a * 31 + k + 107;
        if (a > 1107) { a -= 9; } else { a += 2; }
        a = a * 31 + k + 108;
        if (a > 1108) { a -= 10; } else { a += 0; }
        a = a * 31 + k + 109;
        if (a > 1109) { a -= 11; } else { a += 1; }
        a = a * 31 + k + 110;
        if (a > 1110) { a -= 7; } else { a += 2; }
        a = a * 31 + k + 111;
        if (a > 1111) { a -= 8; } else { a += 0; }
        a = a * 31 + k + 112;
        if (a > 1112) { a -= 9; } else { a += 1; }
        a = a * 31 + k + 113;
        if (a > 1113) { a -= 10; } else { a += 2; }
        a = a * 31 + k + 114;
        if (a > 1114) { a -= 11; } else { a += 0; }
        a = a * 31 + k + 115;
        if (a > 1115) { a -= 7; } else { a += 1; }
        a = a * 31 + k + 116;
        if (a > 1116) { a -= 8; } else { a += 2; }
        a = a * 31 + k + 117;
        if (a > 1117) { a -= 9; } else { a += 0; }
        a = a * 31 + k + 118;
        if (a > 1118) { a -= 10; } else { a += 1; }
        a = a * 31 + k + 119;
        if (a > 1119) { a -= 11; } else { a += 2; }
        return a + n.v;
    }

    static long sink;

    static long run(boolean big, boolean read, int iters) {
        long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) {
            try {
                sink += big ? large(null, i) : small(null);
            } catch (NullPointerException e) {
                if (read) {
                    String m = e.getMessage();
                    sink += m == null ? 0 : m.length();
                }
            }
        }
        return (System.nanoTime() - t0) / iters;
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        String[] names = {"small", "large", "read-small", "read-large"};
        long[][] reps = new long[4][5];
        for (int r = 0; r < 5; r++) {
            reps[0][r] = run(false, false, iters);
            reps[1][r] = run(true, false, iters);
            reps[2][r] = run(false, true, iters);
            reps[3][r] = run(true, true, iters);
        }
        for (int i = 0; i < 4; i++) {
            long[] s = reps[i].clone();
            Arrays.sort(s);
            System.out.println(names[i] + " " + s[2] + " ns");
        }
        try {
            small(null);
        } catch (NullPointerException e) {
            System.out.println("check " + e.getMessage());
        }
    }
}
