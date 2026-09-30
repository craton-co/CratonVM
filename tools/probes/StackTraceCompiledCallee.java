// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * Companion to `StackTraceAfterOsr`: the same throw, but with the callee too
 * BIG to splice, so it is compiled and CALLED rather than inlined.
 *
 * This is the arm the jit-compiled-frame page tried to reach with
 * `CRATONVM_JIT_NO_INLINE=1` — a variable that does not exist (it is not in
 * `types/tests/flag-surface.txt` and nothing reads it), so that arm ran the
 * default configuration and isolated nothing. Passing `MAX_INLINE_BYTECODE_SIZE`
 * (325) does the same job for real, in Java, on any VM.
 *
 * What it is for: a compiled activation of `big` should appear ONCE. If the
 * trace shows `big` twice — once from the interpreter `Frame` and once from the
 * compiled activation — that is the page's fourth issue, an ordinary compiled
 * call not deduped against its interpreter frame.
 *
 *   java -XX:-OmitStackTraceInFastThrow -cp probes StackTraceCompiledCallee
 *   cratonvm -cp probes StackTraceCompiledCallee
 */
public class StackTraceCompiledCallee {
    static int[][] table = new int[8][];
    static int sink;

    /** Padded past the 325-byte FreqInlineSize cap so it is never spliced. */
    static int big(int i) {
        int a = i, b = i + 1, c = i + 2, d = i + 3;
        a += b * 3; b ^= c + 5; c -= d | 7; d += a & 11;
        a += b * 13; b ^= c + 17; c -= d | 19; d += a & 23;
        a += b * 29; b ^= c + 31; c -= d | 37; d += a & 41;
        a += b * 43; b ^= c + 47; c -= d | 53; d += a & 59;
        a += b * 61; b ^= c + 67; c -= d | 71; d += a & 73;
        a += b * 79; b ^= c + 83; c -= d | 89; d += a & 97;
        a += b * 101; b ^= c + 103; c -= d | 107; d += a & 109;
        a += b * 113; b ^= c + 127; c -= d | 131; d += a & 137;
        a += b * 139; b ^= c + 149; c -= d | 151; d += a & 157;
        a += b * 163; b ^= c + 167; c -= d | 173; d += a & 179;
        a += b * 181; b ^= c + 191; c -= d | 193; d += a & 197;
        a += b * 199; b ^= c + 211; c -= d | 223; d += a & 227;
        sink = a + b + c + d;
        return table[i & 7][0];                                  // the throw site
    }

    static String tr(Throwable e) {
        StackTraceElement[] st = e.getStackTrace();
        StringBuilder sb = new StringBuilder("len=").append(st.length).append(" [");
        for (StackTraceElement s : st) {
            sb.append(s.getMethodName()).append(':').append(s.getLineNumber()).append(' ');
        }
        return sb.append(']').toString();
    }

    static String probe() {
        table[1] = null;
        try { big(1); return "no-ex"; }
        catch (NullPointerException e) { return tr(e); }
        finally { table[1] = new int[]{1}; }
    }

    static long warm(int n) { long a = 0; for (int r = 0; r < n; r++) a += big(r); return a; }

    public static void main(String[] args) {
        for (int i = 0; i < 8; i++) table[i] = new int[]{i};
        System.out.println("cold=" + probe());
        System.out.println("warmed=" + (warm(400_000) != 0));
        System.out.println("after_warm=" + probe());
        long a = 0;
        for (int r = 0; r < 400_000; r++) a += r ^ (r >>> 3);
        System.out.println("looped=" + (a != 0));
        System.out.println("after_osr=" + probe());
    }
}
