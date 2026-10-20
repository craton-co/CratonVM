// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 63, lane L7: ns per loop iteration for the
// shapes the two wave-63 switches target.
//
//   CRATONVM_INTERP_FUSED_LDC_BOUND=1  the loop head `iload i; ldc #N;
//       if_icmp<cond>` is one fused dispatch (proposal
//       i62-L7-proposal-the-fused-compare-and-branch-takes-an-ldc-int-bound-20261026).
//   CRATONVM_INTERP_LDC_HOISTED_PC=1   the `ldc` arm stores its pc through
//       the hoisted frame (proposal
//       i58-L7-proposal-the-ldc-arm-sets-the-pc-through-the-hoisted-frame-20261022).
//
// Rows (1 000 000 iterations each unless noted):
//   ldcBound    `i < 1_000_000`: an `ldc` bound (L7W27DispatchLayoutBench's
//               row). FUSED_LDC_BOUND: expected down by about two dispatches.
//   ldcWBound   `i < 1_000_001` with the bound past pool index 255 (`ldc_w`;
//               check with `javap -c`). FUSED_LDC_BOUND: expected down.
//   ldcBody     `s += 100_003` with a local bound: the `ldc` arm itself, not
//               fused (the next opcode is `iadd`). LDC_HOISTED_PC: expected
//               down a little; FUSED_LDC_BOUND: expected flat (the decoder
//               declines on the opcode byte before probing).
//   ldcBoth     `i < 1_000_000` and `s += 100_003 + (i & 3)`: both.
//   sipushBound `i < 1000` inside `j < 1000` (control: already fused).
//   localBound  `i < n` with `n` a parameter (control: already fused).
//
// Run: cratonvm --java-home <jdk25> [--nojit] -cp <dir> L7W63LdcBoundBench,
//      each switch off / on, on the same fat-LTO binary, interleaved, pinned
//      to one core, 5 rounds, medians. Run with --nojit and with the default
//      (JIT on: the loops are OSR-compiled after a few thousand back edges,
//      so the switches should move only the interpreted prefix there).
// stdout is deterministic, identical on HotSpot 25 (and under every switch):
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// ldcBound checksum=1500000
// ldcWBound checksum=1500000
// ldcBody checksum=1218752192
// ldcBoth checksum=1220252192
// sipushBound checksum=1500000
// localBound checksum=1500000
// HOTSPOT_EXPECTED_END
// stderr: `<row> <x> ns/op (min of REPS)`, one line per row.
public class L7W63LdcBoundBench {
    interface Row {
        int run();
    }

    static final int REPS = 5;

    static int ldcBound() {
        int s = 0;
        for (int i = 0; i < 1_000_000; i++) {
            s += i & 3;
        }
        return s;
    }

    static int ldcBody(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += 100_003;
        }
        return s;
    }

    static int ldcBoth() {
        int s = 0;
        for (int i = 0; i < 1_000_000; i++) {
            s += 100_003 + (i & 3);
        }
        return s;
    }

    static int sipushBound() {
        int s = 0;
        for (int j = 0; j < 1000; j++) {
            for (int i = 0; i < 1000; i++) {
                s += i & 3;
            }
        }
        return s;
    }

    static int localBound(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += i & 3;
        }
        return s;
    }

    // Fills the constant pool with 300 Integer entries, so the bound first
    // referenced below this method gets an index past 255 (`ldc_w`).
    static void pad(int[] a) {
        a[0] = 700001;
        a[1] = 700002;
        a[2] = 700003;
        a[3] = 700004;
        a[4] = 700005;
        a[5] = 700006;
        a[6] = 700007;
        a[7] = 700008;
        a[8] = 700009;
        a[9] = 700010;
        a[10] = 700011;
        a[11] = 700012;
        a[12] = 700013;
        a[13] = 700014;
        a[14] = 700015;
        a[15] = 700016;
        a[16] = 700017;
        a[17] = 700018;
        a[18] = 700019;
        a[19] = 700020;
        a[20] = 700021;
        a[21] = 700022;
        a[22] = 700023;
        a[23] = 700024;
        a[24] = 700025;
        a[25] = 700026;
        a[26] = 700027;
        a[27] = 700028;
        a[28] = 700029;
        a[29] = 700030;
        a[30] = 700031;
        a[31] = 700032;
        a[32] = 700033;
        a[33] = 700034;
        a[34] = 700035;
        a[35] = 700036;
        a[36] = 700037;
        a[37] = 700038;
        a[38] = 700039;
        a[39] = 700040;
        a[40] = 700041;
        a[41] = 700042;
        a[42] = 700043;
        a[43] = 700044;
        a[44] = 700045;
        a[45] = 700046;
        a[46] = 700047;
        a[47] = 700048;
        a[48] = 700049;
        a[49] = 700050;
        a[50] = 700051;
        a[51] = 700052;
        a[52] = 700053;
        a[53] = 700054;
        a[54] = 700055;
        a[55] = 700056;
        a[56] = 700057;
        a[57] = 700058;
        a[58] = 700059;
        a[59] = 700060;
        a[60] = 700061;
        a[61] = 700062;
        a[62] = 700063;
        a[63] = 700064;
        a[64] = 700065;
        a[65] = 700066;
        a[66] = 700067;
        a[67] = 700068;
        a[68] = 700069;
        a[69] = 700070;
        a[70] = 700071;
        a[71] = 700072;
        a[72] = 700073;
        a[73] = 700074;
        a[74] = 700075;
        a[75] = 700076;
        a[76] = 700077;
        a[77] = 700078;
        a[78] = 700079;
        a[79] = 700080;
        a[80] = 700081;
        a[81] = 700082;
        a[82] = 700083;
        a[83] = 700084;
        a[84] = 700085;
        a[85] = 700086;
        a[86] = 700087;
        a[87] = 700088;
        a[88] = 700089;
        a[89] = 700090;
        a[90] = 700091;
        a[91] = 700092;
        a[92] = 700093;
        a[93] = 700094;
        a[94] = 700095;
        a[95] = 700096;
        a[96] = 700097;
        a[97] = 700098;
        a[98] = 700099;
        a[99] = 700100;
        a[100] = 700101;
        a[101] = 700102;
        a[102] = 700103;
        a[103] = 700104;
        a[104] = 700105;
        a[105] = 700106;
        a[106] = 700107;
        a[107] = 700108;
        a[108] = 700109;
        a[109] = 700110;
        a[110] = 700111;
        a[111] = 700112;
        a[112] = 700113;
        a[113] = 700114;
        a[114] = 700115;
        a[115] = 700116;
        a[116] = 700117;
        a[117] = 700118;
        a[118] = 700119;
        a[119] = 700120;
        a[120] = 700121;
        a[121] = 700122;
        a[122] = 700123;
        a[123] = 700124;
        a[124] = 700125;
        a[125] = 700126;
        a[126] = 700127;
        a[127] = 700128;
        a[128] = 700129;
        a[129] = 700130;
        a[130] = 700131;
        a[131] = 700132;
        a[132] = 700133;
        a[133] = 700134;
        a[134] = 700135;
        a[135] = 700136;
        a[136] = 700137;
        a[137] = 700138;
        a[138] = 700139;
        a[139] = 700140;
        a[140] = 700141;
        a[141] = 700142;
        a[142] = 700143;
        a[143] = 700144;
        a[144] = 700145;
        a[145] = 700146;
        a[146] = 700147;
        a[147] = 700148;
        a[148] = 700149;
        a[149] = 700150;
        a[150] = 700151;
        a[151] = 700152;
        a[152] = 700153;
        a[153] = 700154;
        a[154] = 700155;
        a[155] = 700156;
        a[156] = 700157;
        a[157] = 700158;
        a[158] = 700159;
        a[159] = 700160;
        a[160] = 700161;
        a[161] = 700162;
        a[162] = 700163;
        a[163] = 700164;
        a[164] = 700165;
        a[165] = 700166;
        a[166] = 700167;
        a[167] = 700168;
        a[168] = 700169;
        a[169] = 700170;
        a[170] = 700171;
        a[171] = 700172;
        a[172] = 700173;
        a[173] = 700174;
        a[174] = 700175;
        a[175] = 700176;
        a[176] = 700177;
        a[177] = 700178;
        a[178] = 700179;
        a[179] = 700180;
        a[180] = 700181;
        a[181] = 700182;
        a[182] = 700183;
        a[183] = 700184;
        a[184] = 700185;
        a[185] = 700186;
        a[186] = 700187;
        a[187] = 700188;
        a[188] = 700189;
        a[189] = 700190;
        a[190] = 700191;
        a[191] = 700192;
        a[192] = 700193;
        a[193] = 700194;
        a[194] = 700195;
        a[195] = 700196;
        a[196] = 700197;
        a[197] = 700198;
        a[198] = 700199;
        a[199] = 700200;
        a[200] = 700201;
        a[201] = 700202;
        a[202] = 700203;
        a[203] = 700204;
        a[204] = 700205;
        a[205] = 700206;
        a[206] = 700207;
        a[207] = 700208;
        a[208] = 700209;
        a[209] = 700210;
        a[210] = 700211;
        a[211] = 700212;
        a[212] = 700213;
        a[213] = 700214;
        a[214] = 700215;
        a[215] = 700216;
        a[216] = 700217;
        a[217] = 700218;
        a[218] = 700219;
        a[219] = 700220;
        a[220] = 700221;
        a[221] = 700222;
        a[222] = 700223;
        a[223] = 700224;
        a[224] = 700225;
        a[225] = 700226;
        a[226] = 700227;
        a[227] = 700228;
        a[228] = 700229;
        a[229] = 700230;
        a[230] = 700231;
        a[231] = 700232;
        a[232] = 700233;
        a[233] = 700234;
        a[234] = 700235;
        a[235] = 700236;
        a[236] = 700237;
        a[237] = 700238;
        a[238] = 700239;
        a[239] = 700240;
        a[240] = 700241;
        a[241] = 700242;
        a[242] = 700243;
        a[243] = 700244;
        a[244] = 700245;
        a[245] = 700246;
        a[246] = 700247;
        a[247] = 700248;
        a[248] = 700249;
        a[249] = 700250;
        a[250] = 700251;
        a[251] = 700252;
        a[252] = 700253;
        a[253] = 700254;
        a[254] = 700255;
        a[255] = 700256;
        a[256] = 700257;
        a[257] = 700258;
        a[258] = 700259;
        a[259] = 700260;
        a[260] = 700261;
        a[261] = 700262;
        a[262] = 700263;
        a[263] = 700264;
        a[264] = 700265;
        a[265] = 700266;
        a[266] = 700267;
        a[267] = 700268;
        a[268] = 700269;
        a[269] = 700270;
        a[270] = 700271;
        a[271] = 700272;
        a[272] = 700273;
        a[273] = 700274;
        a[274] = 700275;
        a[275] = 700276;
        a[276] = 700277;
        a[277] = 700278;
        a[278] = 700279;
        a[279] = 700280;
        a[280] = 700281;
        a[281] = 700282;
        a[282] = 700283;
        a[283] = 700284;
        a[284] = 700285;
        a[285] = 700286;
        a[286] = 700287;
        a[287] = 700288;
        a[288] = 700289;
        a[289] = 700290;
        a[290] = 700291;
        a[291] = 700292;
        a[292] = 700293;
        a[293] = 700294;
        a[294] = 700295;
        a[295] = 700296;
        a[296] = 700297;
        a[297] = 700298;
        a[298] = 700299;
        a[299] = 700300;
    }

    static int ldcWBound() {
        int s = 0;
        for (int i = 0; i < 1_000_001; i++) {
            s += i & 3;
        }
        return s;
    }

    static void row(String name, int iterations, Row r) {
        int checksum = 0;
        long best = Long.MAX_VALUE;
        for (int rep = 0; rep < REPS; rep++) {
            long t0 = System.nanoTime();
            int c = r.run();
            long dt = System.nanoTime() - t0;
            if (dt < best) {
                best = dt;
            }
            if (rep == 0) {
                checksum = c;
            } else if (c != checksum) {
                checksum = -1;
            }
        }
        System.out.println(name + " checksum=" + checksum);
        System.err.printf("%-11s %8.2f ns/op (min of %d)%n", name, (double) best / iterations, REPS);
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 1_000_000;
        row("ldcBound", 1_000_000, L7W63LdcBoundBench::ldcBound);
        row("ldcWBound", 1_000_001, L7W63LdcBoundBench::ldcWBound);
        row("ldcBody", n, () -> ldcBody(n));
        row("ldcBoth", 1_000_000, L7W63LdcBoundBench::ldcBoth);
        row("sipushBound", 1_000_000, L7W63LdcBoundBench::sipushBound);
        row("localBound", n, () -> localBound(n));
    }
}
