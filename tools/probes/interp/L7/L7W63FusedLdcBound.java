// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 63, lane L7: the fused int compare-and-branch
// takes an `ldc` / `ldc_w` int as its second operand
// (`CRATONVM_INTERP_FUSED_LDC_BOUND`, default off; proposal
// docs/internal/fixed-bugs/interpreter-i62-L7-proposal-the-fused-compare-and-branch-takes-an-ldc-int-bound-DONE-20261026.md),
// and the `ldc` arm stores its pc through the hoisted frame
// (`CRATONVM_INTERP_LDC_HOISTED_PC`, default off; proposal
// i58-L7-proposal-the-ldc-arm-sets-the-pc-through-the-hoisted-frame-20261022.md).
// No behaviour change: every line is the same under every combination of
// the two switches, with and without --nojit.
//
// Shapes:
//   ldc     `x < 70000`, `x <= 70000`, `x > -70000`, `x >= -70000`,
//           `x == 123456`, `x != 123456`, `x < Integer.MAX_VALUE`,
//           `x > Integer.MIN_VALUE`: all six `if_icmp<cond>` against an
//           `ldc` bound (`iload_0; ldc; if_icmp<cond>`), 3000 rounds over
//           edge values, one mask per value.
//   ldc_w   the same with bounds first referenced after `pad` has filled the
//           constant pool past index 255, so javac emits `ldc_w` (check with
//           `javap -c`; the output does not depend on it).
//   loops   `i < 1_000_000` (the `ldcBound` row), the same on a fresh
//           thread (an empty per-thread table: the first test misses and the
//           unfused `ldc` fills it), a counter past local 3 (`iload n`), a
//           count-down to a negative `ldc` bound, an `ldc_w` bound, and an
//           `ldc` in the loop body (`s += 100_003`, not a compare: the
//           fusion must decline and the plain `ldc` arm runs).
//
// Positive control (debug cratonvm, --nojit, CRATONVM_DBG_FIELD_SITE=1 and
// CRATONVM_QUICKEN_STATS=pairs): with CRATONVM_INTERP_FUSED_LDC_BOUND=1 the
// `ldc -> if_icmp*` pairs drop to the first execution of each site, and
// `ldc: hit=` stays about the same (a fused hit counts as the `ldc` hit it
// replaces).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// ldc -2147483648 99
// ldc -80000 227
// ldc -70001 227
// ldc -70000 235
// ldc -69999 239
// ldc 0 239
// ldc 69999 239
// ldc 70000 238
// ldc 70001 236
// ldc 80000 236
// ldc 123456 220
// ldc 234567 236
// ldc 2147483647 172
// ldc total 8529000
// ldc_w -2147483648 99
// ldc_w -80000 235
// ldc_w -70001 239
// ldc_w -70000 239
// ldc_w -69999 239
// ldc_w 0 239
// ldc_w 69999 239
// ldc_w 70000 239
// ldc_w 70001 239
// ldc_w 80000 238
// ldc_w 123456 236
// ldc_w 234567 220
// ldc_w 2147483647 172
// ldc_w total 8619000
// loop ldc-bound 1500000
// loop ldc-bound-thread 1500000
// loop iload-n-bound 300000
// loop countdown 142858
// loop ldc_w-bound 617283
// loop ldc-body -1474236480
// HOTSPOT_EXPECTED_END
public class L7W63FusedLdcBound {
    static int conds(int x) {
        int m = 0;
        if (x < 70000) m |= 1;
        if (x <= 70000) m |= 2;
        if (x > -70000) m |= 4;
        if (x >= -70000) m |= 8;
        if (x == 123456) m |= 16;
        if (x != 123456) m |= 32;
        if (x < 2147483647) m |= 64;
        if (x > -2147483648) m |= 128;
        return m;
    }

    static int ldcBound() {
        int s = 0;
        for (int i = 0; i < 1_000_000; i++) {
            s += i & 3;
        }
        return s;
    }

    static int countdown() {
        int n = 0;
        for (int i = 500000; i > -500000; i -= 7) {
            n++;
        }
        return n;
    }

    static int ldcBody(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += 100_003;
        }
        return s;
    }

    static int iloadN(int a, int b, int c, int d) {
        int s = 0;
        for (int i = a; i < 300000; i++) {
            s += b;
        }
        return s + c + d;
    }

    // Fills the constant pool with 300 Integer entries, so every int
    // constant first referenced below this method gets an index past 255.
    static void pad(int[] a) {
        a[0] = 500001;
        a[1] = 500002;
        a[2] = 500003;
        a[3] = 500004;
        a[4] = 500005;
        a[5] = 500006;
        a[6] = 500007;
        a[7] = 500008;
        a[8] = 500009;
        a[9] = 500010;
        a[10] = 500011;
        a[11] = 500012;
        a[12] = 500013;
        a[13] = 500014;
        a[14] = 500015;
        a[15] = 500016;
        a[16] = 500017;
        a[17] = 500018;
        a[18] = 500019;
        a[19] = 500020;
        a[20] = 500021;
        a[21] = 500022;
        a[22] = 500023;
        a[23] = 500024;
        a[24] = 500025;
        a[25] = 500026;
        a[26] = 500027;
        a[27] = 500028;
        a[28] = 500029;
        a[29] = 500030;
        a[30] = 500031;
        a[31] = 500032;
        a[32] = 500033;
        a[33] = 500034;
        a[34] = 500035;
        a[35] = 500036;
        a[36] = 500037;
        a[37] = 500038;
        a[38] = 500039;
        a[39] = 500040;
        a[40] = 500041;
        a[41] = 500042;
        a[42] = 500043;
        a[43] = 500044;
        a[44] = 500045;
        a[45] = 500046;
        a[46] = 500047;
        a[47] = 500048;
        a[48] = 500049;
        a[49] = 500050;
        a[50] = 500051;
        a[51] = 500052;
        a[52] = 500053;
        a[53] = 500054;
        a[54] = 500055;
        a[55] = 500056;
        a[56] = 500057;
        a[57] = 500058;
        a[58] = 500059;
        a[59] = 500060;
        a[60] = 500061;
        a[61] = 500062;
        a[62] = 500063;
        a[63] = 500064;
        a[64] = 500065;
        a[65] = 500066;
        a[66] = 500067;
        a[67] = 500068;
        a[68] = 500069;
        a[69] = 500070;
        a[70] = 500071;
        a[71] = 500072;
        a[72] = 500073;
        a[73] = 500074;
        a[74] = 500075;
        a[75] = 500076;
        a[76] = 500077;
        a[77] = 500078;
        a[78] = 500079;
        a[79] = 500080;
        a[80] = 500081;
        a[81] = 500082;
        a[82] = 500083;
        a[83] = 500084;
        a[84] = 500085;
        a[85] = 500086;
        a[86] = 500087;
        a[87] = 500088;
        a[88] = 500089;
        a[89] = 500090;
        a[90] = 500091;
        a[91] = 500092;
        a[92] = 500093;
        a[93] = 500094;
        a[94] = 500095;
        a[95] = 500096;
        a[96] = 500097;
        a[97] = 500098;
        a[98] = 500099;
        a[99] = 500100;
        a[100] = 500101;
        a[101] = 500102;
        a[102] = 500103;
        a[103] = 500104;
        a[104] = 500105;
        a[105] = 500106;
        a[106] = 500107;
        a[107] = 500108;
        a[108] = 500109;
        a[109] = 500110;
        a[110] = 500111;
        a[111] = 500112;
        a[112] = 500113;
        a[113] = 500114;
        a[114] = 500115;
        a[115] = 500116;
        a[116] = 500117;
        a[117] = 500118;
        a[118] = 500119;
        a[119] = 500120;
        a[120] = 500121;
        a[121] = 500122;
        a[122] = 500123;
        a[123] = 500124;
        a[124] = 500125;
        a[125] = 500126;
        a[126] = 500127;
        a[127] = 500128;
        a[128] = 500129;
        a[129] = 500130;
        a[130] = 500131;
        a[131] = 500132;
        a[132] = 500133;
        a[133] = 500134;
        a[134] = 500135;
        a[135] = 500136;
        a[136] = 500137;
        a[137] = 500138;
        a[138] = 500139;
        a[139] = 500140;
        a[140] = 500141;
        a[141] = 500142;
        a[142] = 500143;
        a[143] = 500144;
        a[144] = 500145;
        a[145] = 500146;
        a[146] = 500147;
        a[147] = 500148;
        a[148] = 500149;
        a[149] = 500150;
        a[150] = 500151;
        a[151] = 500152;
        a[152] = 500153;
        a[153] = 500154;
        a[154] = 500155;
        a[155] = 500156;
        a[156] = 500157;
        a[157] = 500158;
        a[158] = 500159;
        a[159] = 500160;
        a[160] = 500161;
        a[161] = 500162;
        a[162] = 500163;
        a[163] = 500164;
        a[164] = 500165;
        a[165] = 500166;
        a[166] = 500167;
        a[167] = 500168;
        a[168] = 500169;
        a[169] = 500170;
        a[170] = 500171;
        a[171] = 500172;
        a[172] = 500173;
        a[173] = 500174;
        a[174] = 500175;
        a[175] = 500176;
        a[176] = 500177;
        a[177] = 500178;
        a[178] = 500179;
        a[179] = 500180;
        a[180] = 500181;
        a[181] = 500182;
        a[182] = 500183;
        a[183] = 500184;
        a[184] = 500185;
        a[185] = 500186;
        a[186] = 500187;
        a[187] = 500188;
        a[188] = 500189;
        a[189] = 500190;
        a[190] = 500191;
        a[191] = 500192;
        a[192] = 500193;
        a[193] = 500194;
        a[194] = 500195;
        a[195] = 500196;
        a[196] = 500197;
        a[197] = 500198;
        a[198] = 500199;
        a[199] = 500200;
        a[200] = 500201;
        a[201] = 500202;
        a[202] = 500203;
        a[203] = 500204;
        a[204] = 500205;
        a[205] = 500206;
        a[206] = 500207;
        a[207] = 500208;
        a[208] = 500209;
        a[209] = 500210;
        a[210] = 500211;
        a[211] = 500212;
        a[212] = 500213;
        a[213] = 500214;
        a[214] = 500215;
        a[215] = 500216;
        a[216] = 500217;
        a[217] = 500218;
        a[218] = 500219;
        a[219] = 500220;
        a[220] = 500221;
        a[221] = 500222;
        a[222] = 500223;
        a[223] = 500224;
        a[224] = 500225;
        a[225] = 500226;
        a[226] = 500227;
        a[227] = 500228;
        a[228] = 500229;
        a[229] = 500230;
        a[230] = 500231;
        a[231] = 500232;
        a[232] = 500233;
        a[233] = 500234;
        a[234] = 500235;
        a[235] = 500236;
        a[236] = 500237;
        a[237] = 500238;
        a[238] = 500239;
        a[239] = 500240;
        a[240] = 500241;
        a[241] = 500242;
        a[242] = 500243;
        a[243] = 500244;
        a[244] = 500245;
        a[245] = 500246;
        a[246] = 500247;
        a[247] = 500248;
        a[248] = 500249;
        a[249] = 500250;
        a[250] = 500251;
        a[251] = 500252;
        a[252] = 500253;
        a[253] = 500254;
        a[254] = 500255;
        a[255] = 500256;
        a[256] = 500257;
        a[257] = 500258;
        a[258] = 500259;
        a[259] = 500260;
        a[260] = 500261;
        a[261] = 500262;
        a[262] = 500263;
        a[263] = 500264;
        a[264] = 500265;
        a[265] = 500266;
        a[266] = 500267;
        a[267] = 500268;
        a[268] = 500269;
        a[269] = 500270;
        a[270] = 500271;
        a[271] = 500272;
        a[272] = 500273;
        a[273] = 500274;
        a[274] = 500275;
        a[275] = 500276;
        a[276] = 500277;
        a[277] = 500278;
        a[278] = 500279;
        a[279] = 500280;
        a[280] = 500281;
        a[281] = 500282;
        a[282] = 500283;
        a[283] = 500284;
        a[284] = 500285;
        a[285] = 500286;
        a[286] = 500287;
        a[287] = 500288;
        a[288] = 500289;
        a[289] = 500290;
        a[290] = 500291;
        a[291] = 500292;
        a[292] = 500293;
        a[293] = 500294;
        a[294] = 500295;
        a[295] = 500296;
        a[296] = 500297;
        a[297] = 500298;
        a[298] = 500299;
        a[299] = 500300;
    }

    static int condsW(int x) {
        int m = 0;
        if (x < 80000) m |= 1;
        if (x <= 80000) m |= 2;
        if (x > -80000) m |= 4;
        if (x >= -80000) m |= 8;
        if (x == 234567) m |= 16;
        if (x != 234567) m |= 32;
        if (x < 2147483647) m |= 64;
        if (x > -2147483648) m |= 128;
        return m;
    }

    static int ldcWBound() {
        int s = 0;
        for (int i = 0; i < 1_234_567; i++) {
            s += i & 1;
        }
        return s;
    }

    public static void main(String[] args) throws Exception {
        pad(new int[300]);
        // Built here (in `main`, after `condsW` in the source) rather than in
        // a static initializer, so that `condsW`'s bounds are first
        // referenced by `condsW` itself and land past pool index 255.
        final int[] VALUES = {
            Integer.MIN_VALUE, -80000, -70001, -70000, -69999, 0, 69999, 70000, 70001, 80000,
            123456, 234567, Integer.MAX_VALUE
        };
        long total = 0;
        long totalW = 0;
        int[] masks = new int[VALUES.length];
        int[] masksW = new int[VALUES.length];
        for (int r = 0; r < 3000; r++) {
            for (int k = 0; k < VALUES.length; k++) {
                masks[k] = conds(VALUES[k]);
                masksW[k] = condsW(VALUES[k]);
                total += masks[k];
                totalW += masksW[k];
            }
        }
        for (int k = 0; k < VALUES.length; k++) {
            System.out.println("ldc " + VALUES[k] + " " + masks[k]);
        }
        System.out.println("ldc total " + total);
        for (int k = 0; k < VALUES.length; k++) {
            System.out.println("ldc_w " + VALUES[k] + " " + masksW[k]);
        }
        System.out.println("ldc_w total " + totalW);
        System.out.println("loop ldc-bound " + ldcBound());
        int[] fromThread = new int[1];
        Thread t = new Thread(() -> fromThread[0] = ldcBound());
        t.start();
        t.join();
        System.out.println("loop ldc-bound-thread " + fromThread[0]);
        System.out.println("loop iload-n-bound " + iloadN(0, 1, 0, 0));
        System.out.println("loop countdown " + countdown());
        System.out.println("loop ldc_w-bound " + ldcWBound());
        System.out.println("loop ldc-body " + ldcBody(200_000));
    }
}
