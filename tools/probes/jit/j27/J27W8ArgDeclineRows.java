// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

// j27 wave 8 (lane W8ARG): the DECLINE arms of a callee trap at a direct call,
// driven on purpose with the debug lever CRATONVM_DBG_FORCE_CALLEE_DECLINE.
// Pages: docs/feature-designs/native-dispatch-architecture.md,
//        docs/feature-designs/native-dispatch-architecture.md.
//
// A declined stash at an owned-arguments site leaves as the CALLER's
// frameless deopt, and the caller's caller re-runs the caller from bci 0
// (FramelessTrapSite::Handoff). Under the two-level rule
// (CRATONVM_JIT_IR_CALLEE_OWNED_ARGS_SPLICE_REPLAY=1) the caller is the OUTER
// method of a splice. At a copying site the service re-runs the callee from
// its argument copies instead. Either way every effect must happen once.
//
// Four chains, each `outerX` (padded over the 325-byte splice limit, no loop)
// -> `midX` (tiny, spliced) -> `leafX` (padded, a real direct call from inside
// the splice). Each leaf traps on its first branch, never taken while warming,
// BEFORE it commits anything, so the callee's own replay is exact:
//   S  pure prefixes everywhere: the two-level rule admits the site
//      (`splice-replay=Ok(184)`); called once from main;
//   C  `stamp = 7;` before the call in outerC: refused (`Err(Commit(0))`);
//   M  `midCount++;` in midM before its call: refused (`Err(Commit(1))`);
//   D  the S shape, called from a warm compiled loop (`drive`), so the
//      re-run is asked by a compiled caller's site.
//
//   java -Xmx256m -cp <dir> J27W8ArgDeclineRows > hs.txt
//   cratonvm -Xmx256m --cp <dir> J27W8ArgDeclineRows | diff - hs.txt
//     x {default, CRATONVM_JIT_IR_CALLEE_OWNED_ARGS_SPLICE_REPLAY=1,
//        CRATONVM_JIT_IR_CALLEE_OWNED_ARGS=0}
//     x {unset, CRATONVM_DBG_FORCE_CALLEE_DECLINE=J27W8ArgDeclineRows.leaf}
//     x {--real-jdk, --compatible}
//   Every fact SAME as HotSpot in every arm.
//   engagement: + CRATONVM_DBG_DEOPT=1 CRATONVM_DBG_IR_SLOTS=1 2>&1 | grep -E 'forced by|owned-arguments call site|ir-owned-args\] site.*leaf'
//     forced arm: `callee-resume refused (forced by CRATONVM_DBG_FORCE_CALLEE_DECLINE): J27W8ArgDeclineRows.leafS:([II)I`
//     with ..._SPLICE_REPLAY=1: `site bci=<pc> J27W8ArgDeclineRows.leafS([II)I kind=3 admitted=true owned=true splice-replay=Ok(184)`
//       and `declined callee trap dropped at an owned-arguments call site: J27W8ArgDeclineRows.leafS([II)I ... callee replay=exact`
//     leafC / leafM lines: `splice-replay=Err(Commit(0))` / `Err(Commit(1))`.
//   If the forced line never prints for leafS, the trap was not a stash (no
//   decline to force): note it and read the row as not engaged.
//
// Expected HotSpot 25 output:
//   fact warm w=357000/357000/357000/357000 a0=42000/42000/42000/42000 effects=168000 stamp=7 midCount=42000
//   fact decline-spliced r=4008 effects=100 a0=7
//   fact decline-outer-commit r=4008 effects=100 a0=7 stamp=7
//   fact decline-mid-commit r=4008 effects=100 a0=7 midCount=1
//   fact decline-driven w=22499 effects=2099 a0=2006
//   fact after w=8500 effects=1000 a0=1000
//   fact pad 0
public class J27W8ArgDeclineRows {
    static int effects;
    static int stamp;
    static int midCount;
    static long pad;

    static int outerS(int[] a, int k) {
        int r = midS(a, k);
        if (r < -1000000) {
            // Never taken: keeps the method over the 325-byte splice limit.
            pad = pad * 31L + 1L;
            pad = pad * 33L + 2L;
            pad = pad * 35L + 3L;
            pad = pad * 37L + 4L;
            pad = pad * 39L + 5L;
            pad = pad * 41L + 6L;
            pad = pad * 43L + 7L;
            pad = pad * 45L + 8L;
            pad = pad * 47L + 9L;
            pad = pad * 49L + 10L;
            pad = pad * 51L + 11L;
            pad = pad * 53L + 12L;
            pad = pad * 55L + 13L;
            pad = pad * 57L + 14L;
            pad = pad * 59L + 15L;
            pad = pad * 61L + 16L;
            pad = pad * 63L + 17L;
            pad = pad * 65L + 18L;
            pad = pad * 67L + 19L;
            pad = pad * 69L + 20L;
            pad = pad * 71L + 21L;
            pad = pad * 73L + 22L;
            pad = pad * 75L + 23L;
            pad = pad * 77L + 24L;
            pad = pad * 79L + 25L;
            pad = pad * 81L + 26L;
            pad = pad * 83L + 27L;
            pad = pad * 85L + 28L;
            pad = pad * 87L + 29L;
            pad = pad * 89L + 30L;
            pad = pad * 91L + 31L;
            pad = pad * 93L + 32L;
        }
        return r;
    }

    static int midS(int[] a, int k) {
        return leafS(a, k) + 1;
    }

    static int leafS(int[] a, int k) {
        if (k == -2) {
            // Never taken while warming: the trap fires here, before the
            // callee commits anything, so its own replay is exact.
            effects += 100;
            a[0] += 7;
            return a.length * 1000 + a[0];
        }
        effects++;
        a[0] += 1;
        if (k < -1000) {
            // Never taken: keeps the method over the 325-byte splice limit.
            pad = pad * 31L + 1L;
            pad = pad * 33L + 2L;
            pad = pad * 35L + 3L;
            pad = pad * 37L + 4L;
            pad = pad * 39L + 5L;
            pad = pad * 41L + 6L;
            pad = pad * 43L + 7L;
            pad = pad * 45L + 8L;
            pad = pad * 47L + 9L;
            pad = pad * 49L + 10L;
            pad = pad * 51L + 11L;
            pad = pad * 53L + 12L;
            pad = pad * 55L + 13L;
            pad = pad * 57L + 14L;
            pad = pad * 59L + 15L;
            pad = pad * 61L + 16L;
            pad = pad * 63L + 17L;
            pad = pad * 65L + 18L;
            pad = pad * 67L + 19L;
            pad = pad * 69L + 20L;
            pad = pad * 71L + 21L;
            pad = pad * 73L + 22L;
            pad = pad * 75L + 23L;
            pad = pad * 77L + 24L;
            pad = pad * 79L + 25L;
            pad = pad * 81L + 26L;
            pad = pad * 83L + 27L;
            pad = pad * 85L + 28L;
            pad = pad * 87L + 29L;
            pad = pad * 89L + 30L;
            pad = pad * 91L + 31L;
            pad = pad * 93L + 32L;
        }
        return a.length + k;
    }

    static int outerC(int[] a, int k) {
        stamp = 7;
        int r = midC(a, k);
        if (r < -1000000) {
            // Never taken: keeps the method over the 325-byte splice limit.
            pad = pad * 31L + 1L;
            pad = pad * 33L + 2L;
            pad = pad * 35L + 3L;
            pad = pad * 37L + 4L;
            pad = pad * 39L + 5L;
            pad = pad * 41L + 6L;
            pad = pad * 43L + 7L;
            pad = pad * 45L + 8L;
            pad = pad * 47L + 9L;
            pad = pad * 49L + 10L;
            pad = pad * 51L + 11L;
            pad = pad * 53L + 12L;
            pad = pad * 55L + 13L;
            pad = pad * 57L + 14L;
            pad = pad * 59L + 15L;
            pad = pad * 61L + 16L;
            pad = pad * 63L + 17L;
            pad = pad * 65L + 18L;
            pad = pad * 67L + 19L;
            pad = pad * 69L + 20L;
            pad = pad * 71L + 21L;
            pad = pad * 73L + 22L;
            pad = pad * 75L + 23L;
            pad = pad * 77L + 24L;
            pad = pad * 79L + 25L;
            pad = pad * 81L + 26L;
            pad = pad * 83L + 27L;
            pad = pad * 85L + 28L;
            pad = pad * 87L + 29L;
            pad = pad * 89L + 30L;
            pad = pad * 91L + 31L;
            pad = pad * 93L + 32L;
        }
        return r;
    }

    static int midC(int[] a, int k) {
        return leafC(a, k) + 1;
    }

    static int leafC(int[] a, int k) {
        if (k == -2) {
            // Never taken while warming: the trap fires here, before the
            // callee commits anything, so its own replay is exact.
            effects += 100;
            a[0] += 7;
            return a.length * 1000 + a[0];
        }
        effects++;
        a[0] += 1;
        if (k < -1000) {
            // Never taken: keeps the method over the 325-byte splice limit.
            pad = pad * 31L + 1L;
            pad = pad * 33L + 2L;
            pad = pad * 35L + 3L;
            pad = pad * 37L + 4L;
            pad = pad * 39L + 5L;
            pad = pad * 41L + 6L;
            pad = pad * 43L + 7L;
            pad = pad * 45L + 8L;
            pad = pad * 47L + 9L;
            pad = pad * 49L + 10L;
            pad = pad * 51L + 11L;
            pad = pad * 53L + 12L;
            pad = pad * 55L + 13L;
            pad = pad * 57L + 14L;
            pad = pad * 59L + 15L;
            pad = pad * 61L + 16L;
            pad = pad * 63L + 17L;
            pad = pad * 65L + 18L;
            pad = pad * 67L + 19L;
            pad = pad * 69L + 20L;
            pad = pad * 71L + 21L;
            pad = pad * 73L + 22L;
            pad = pad * 75L + 23L;
            pad = pad * 77L + 24L;
            pad = pad * 79L + 25L;
            pad = pad * 81L + 26L;
            pad = pad * 83L + 27L;
            pad = pad * 85L + 28L;
            pad = pad * 87L + 29L;
            pad = pad * 89L + 30L;
            pad = pad * 91L + 31L;
            pad = pad * 93L + 32L;
        }
        return a.length + k;
    }

    static int outerM(int[] a, int k) {
        int r = midM(a, k);
        if (r < -1000000) {
            // Never taken: keeps the method over the 325-byte splice limit.
            pad = pad * 31L + 1L;
            pad = pad * 33L + 2L;
            pad = pad * 35L + 3L;
            pad = pad * 37L + 4L;
            pad = pad * 39L + 5L;
            pad = pad * 41L + 6L;
            pad = pad * 43L + 7L;
            pad = pad * 45L + 8L;
            pad = pad * 47L + 9L;
            pad = pad * 49L + 10L;
            pad = pad * 51L + 11L;
            pad = pad * 53L + 12L;
            pad = pad * 55L + 13L;
            pad = pad * 57L + 14L;
            pad = pad * 59L + 15L;
            pad = pad * 61L + 16L;
            pad = pad * 63L + 17L;
            pad = pad * 65L + 18L;
            pad = pad * 67L + 19L;
            pad = pad * 69L + 20L;
            pad = pad * 71L + 21L;
            pad = pad * 73L + 22L;
            pad = pad * 75L + 23L;
            pad = pad * 77L + 24L;
            pad = pad * 79L + 25L;
            pad = pad * 81L + 26L;
            pad = pad * 83L + 27L;
            pad = pad * 85L + 28L;
            pad = pad * 87L + 29L;
            pad = pad * 89L + 30L;
            pad = pad * 91L + 31L;
            pad = pad * 93L + 32L;
        }
        return r;
    }

    static int midM(int[] a, int k) {
        midCount++;
        return leafM(a, k) + 1;
    }

    static int leafM(int[] a, int k) {
        if (k == -2) {
            // Never taken while warming: the trap fires here, before the
            // callee commits anything, so its own replay is exact.
            effects += 100;
            a[0] += 7;
            return a.length * 1000 + a[0];
        }
        effects++;
        a[0] += 1;
        if (k < -1000) {
            // Never taken: keeps the method over the 325-byte splice limit.
            pad = pad * 31L + 1L;
            pad = pad * 33L + 2L;
            pad = pad * 35L + 3L;
            pad = pad * 37L + 4L;
            pad = pad * 39L + 5L;
            pad = pad * 41L + 6L;
            pad = pad * 43L + 7L;
            pad = pad * 45L + 8L;
            pad = pad * 47L + 9L;
            pad = pad * 49L + 10L;
            pad = pad * 51L + 11L;
            pad = pad * 53L + 12L;
            pad = pad * 55L + 13L;
            pad = pad * 57L + 14L;
            pad = pad * 59L + 15L;
            pad = pad * 61L + 16L;
            pad = pad * 63L + 17L;
            pad = pad * 65L + 18L;
            pad = pad * 67L + 19L;
            pad = pad * 69L + 20L;
            pad = pad * 71L + 21L;
            pad = pad * 73L + 22L;
            pad = pad * 75L + 23L;
            pad = pad * 77L + 24L;
            pad = pad * 79L + 25L;
            pad = pad * 81L + 26L;
            pad = pad * 83L + 27L;
            pad = pad * 85L + 28L;
            pad = pad * 87L + 29L;
            pad = pad * 89L + 30L;
            pad = pad * 91L + 31L;
            pad = pad * 93L + 32L;
        }
        return a.length + k;
    }

    static int outerD(int[] a, int k) {
        int r = midD(a, k);
        if (r < -1000000) {
            // Never taken: keeps the method over the 325-byte splice limit.
            pad = pad * 31L + 1L;
            pad = pad * 33L + 2L;
            pad = pad * 35L + 3L;
            pad = pad * 37L + 4L;
            pad = pad * 39L + 5L;
            pad = pad * 41L + 6L;
            pad = pad * 43L + 7L;
            pad = pad * 45L + 8L;
            pad = pad * 47L + 9L;
            pad = pad * 49L + 10L;
            pad = pad * 51L + 11L;
            pad = pad * 53L + 12L;
            pad = pad * 55L + 13L;
            pad = pad * 57L + 14L;
            pad = pad * 59L + 15L;
            pad = pad * 61L + 16L;
            pad = pad * 63L + 17L;
            pad = pad * 65L + 18L;
            pad = pad * 67L + 19L;
            pad = pad * 69L + 20L;
            pad = pad * 71L + 21L;
            pad = pad * 73L + 22L;
            pad = pad * 75L + 23L;
            pad = pad * 77L + 24L;
            pad = pad * 79L + 25L;
            pad = pad * 81L + 26L;
            pad = pad * 83L + 27L;
            pad = pad * 85L + 28L;
            pad = pad * 87L + 29L;
            pad = pad * 89L + 30L;
            pad = pad * 91L + 31L;
            pad = pad * 93L + 32L;
        }
        return r;
    }

    static int midD(int[] a, int k) {
        return leafD(a, k) + 1;
    }

    static int leafD(int[] a, int k) {
        if (k == -2) {
            // Never taken while warming: the trap fires here, before the
            // callee commits anything, so its own replay is exact.
            effects += 100;
            a[0] += 7;
            return a.length * 1000 + a[0];
        }
        effects++;
        a[0] += 1;
        if (k < -1000) {
            // Never taken: keeps the method over the 325-byte splice limit.
            pad = pad * 31L + 1L;
            pad = pad * 33L + 2L;
            pad = pad * 35L + 3L;
            pad = pad * 37L + 4L;
            pad = pad * 39L + 5L;
            pad = pad * 41L + 6L;
            pad = pad * 43L + 7L;
            pad = pad * 45L + 8L;
            pad = pad * 47L + 9L;
            pad = pad * 49L + 10L;
            pad = pad * 51L + 11L;
            pad = pad * 53L + 12L;
            pad = pad * 55L + 13L;
            pad = pad * 57L + 14L;
            pad = pad * 59L + 15L;
            pad = pad * 61L + 16L;
            pad = pad * 63L + 17L;
            pad = pad * 65L + 18L;
            pad = pad * 67L + 19L;
            pad = pad * 69L + 20L;
            pad = pad * 71L + 21L;
            pad = pad * 73L + 22L;
            pad = pad * 75L + 23L;
            pad = pad * 77L + 24L;
            pad = pad * 79L + 25L;
            pad = pad * 81L + 26L;
            pad = pad * 83L + 27L;
            pad = pad * 85L + 28L;
            pad = pad * 87L + 29L;
            pad = pad * 89L + 30L;
            pad = pad * 91L + 31L;
            pad = pad * 93L + 32L;
        }
        return a.length + k;
    }

    static long warmS(int[] a, int calls) {
        long w = 0;
        for (int i = 0; i < calls; i++) {
            w += outerS(a, i & 7);
        }
        return w;
    }

    static long warmC(int[] a, int calls) {
        long w = 0;
        for (int i = 0; i < calls; i++) {
            w += outerC(a, i & 7);
        }
        return w;
    }

    static long warmM(int[] a, int calls) {
        long w = 0;
        for (int i = 0; i < calls; i++) {
            w += outerM(a, i & 7);
        }
        return w;
    }

    // The argument comes from an array, so the special call is no branch of
    // this loop (a pruned branch here would trap in `drive` instead).
    static long drive(int[] a, int[] ks) {
        long w = 0;
        for (int i = 0; i < ks.length; i++) {
            w += outerD(a, ks[i]);
        }
        return w;
    }

    public static void main(String[] args) throws InterruptedException {
        int[] aS = new int[4];
        int[] aC = new int[4];
        int[] aM = new int[4];
        int[] aD = new int[4];
        int[] ks = new int[2000];
        for (int i = 0; i < ks.length; i++) {
            ks[i] = i & 7;
        }
        long wS = warmS(aS, 41000);
        long wC = warmC(aC, 41000);
        long wM = warmM(aM, 41000);
        long wD = 0;
        for (int n = 0; n < 20; n++) {
            wD += drive(aD, ks);
        }
        Thread.sleep(300);
        wS += warmS(aS, 1000);
        wC += warmC(aC, 1000);
        wM += warmM(aM, 1000);
        wD += drive(aD, ks);
        System.out.println("fact warm w=" + wS + "/" + wC + "/" + wM + "/" + wD
                + " a0=" + aS[0] + "/" + aC[0] + "/" + aM[0] + "/" + aD[0]
                + " effects=" + effects + " stamp=" + stamp + " midCount=" + midCount);

        effects = 0;
        int[] a = new int[4];
        int r = outerS(a, -2);
        System.out.println("fact decline-spliced r=" + r + " effects=" + effects + " a0=" + a[0]);

        effects = 0;
        stamp = 0;
        a = new int[4];
        r = outerC(a, -2);
        System.out.println("fact decline-outer-commit r=" + r + " effects=" + effects + " a0=" + a[0]
                + " stamp=" + stamp);

        effects = 0;
        midCount = 0;
        a = new int[4];
        r = outerM(a, -2);
        System.out.println("fact decline-mid-commit r=" + r + " effects=" + effects + " a0=" + a[0]
                + " midCount=" + midCount);

        effects = 0;
        a = new int[4];
        int[] ks2 = ks.clone();
        ks2[1500] = -2;
        long w = drive(a, ks2);
        System.out.println("fact decline-driven w=" + w + " effects=" + effects + " a0=" + a[0]);

        effects = 0;
        a = new int[4];
        long after = warmS(a, 1000);
        System.out.println("fact after w=" + after + " effects=" + effects + " a0=" + a[0]);
        System.out.println("fact pad " + pad);
    }
}
