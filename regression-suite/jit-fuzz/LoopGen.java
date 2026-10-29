// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// LoopGen -- FUZZ21-6: a generator of deterministic random loop programs.
//
// Usage (JDK 11+ source launcher, no compile step):
//   java LoopGen.java <seed> <count> <outdir> [kernels=3] [osrN=50000] [warm=20000]
//
// Writes <count> single-file programs <outdir>/R21FuzzGen<seed+i>.java and prints
// each class name on stdout. Same seed, same files, on every JDK and OS.
//
// Each program has <kernels> static kernels `kK(int[] a, long[] b, Base o, int n,
// int p, int q)`: one outer loop over i0 in [lo, n) (forward, strided, backward or
// a while loop) with up to two nested inner loops (small int trip counts, int IVs
// just below Integer.MAX_VALUE, long IVs near Long.MAX_VALUE / MIN_VALUE whose
// bound may wrap to a zero-trip loop), whose bodies mix
//   * int / long / double arithmetic (shifts, odd-divisor division, casts to
//     byte / short / char, rotates, ternaries),
//   * int[] / long[] reads and writes at always-in-bounds indices,
//   * static (S0, S1, S2) and instance field (o.f0, o.g0) reads and writes,
//   * virtual calls o.v(..) on Base / Sub1 / Sub2 and static helper calls,
//   * division / remainder by a possibly-zero divisor and a[i0 + q] accesses,
//     each caught per statement, caught around a whole inner loop (the catch
//     reads the last iteration index), or uncaught (the exception leaves the
//     kernel and `run` folds its class name into the checksum),
//   * conditional `break`s.
// `main` runs each kernel in five phases and prints one `fact` line per phase:
//   osr    -- one cold call with n = osrN (the outer loop OSR-compiles);
//   warm   -- <warm> calls with n = 8 and a monomorphic Sub1 receiver;
//   late   -- one call with a Sub2 receiver (a class the compiled body has never
//             seen), p = n - 1 and q = 1, so `i0 - p` is zero and `a[i0 + q]` is
//             out of bounds on the LAST outer iteration only;
//   again  -- one long call with Sub1 and the same rare-path p / q;
//   mixed  -- 2000 calls with alternating receivers and p / q.
// Every fact folds the kernel's result, the arrays' contents and o.f0. Nothing
// prints an exception message or an identity hash, so the lines are a pure
// function of the Java semantics: HotSpot (default and -Xint), CratonVM
// --nojit and every CratonVM tier must print the same text.
//
// Variants (FUZZTOOL21-2), only with the 7th argument `variants=1` (`0`, the
// default, writes byte-identical programs to the generator before it: no
// extra random draw is taken). The programs are then named R21FuzzGenV<seed>
// and add, per kernel K:
//   * a late-loaded subclass: `LateK extends Sub1` overrides `v` and `w`, a
//     method no loaded class overrides before it (CHA sees one target); it is
//     loaded by `Class.forName` (the verifier never loads it) at one outer
//     iteration of a long call (n > 1000: the osr and again phases), mid-loop,
//     and becomes the receiver `o` from then on;
//   * a late <clinit>: `InitK.X` / `InitK.T[..]` read inside the hot loop
//     behind a flag `lt` that turns on at the same iteration, so the loop is
//     compiled before the class is initialised; <clinit> also writes S2;
//   * `ng`, an int local negative at every OSR entry (-1 - (n & 7)), passed to
//     the helpers below (the round-21 zero-extended OSR seed defect);
//   * long / int mixing across call (splice) boundaries: h2(int, long) -> long
//     and h3(long, int) -> int, with `ng` and arbitrary int expressions;
//   * exceptions thrown by a callee (hx) and by `x / (ng + 1 + (i & 1))`,
//     caught inside the loop on every other iteration.
// Long-call-only parts are inert when osrN <= 1000.
//
// The driver is regression-suite/jit-loopgen.sh (`-V` sets variants=1).

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.ArrayList;
import java.util.List;

public final class LoopGen {
    public static void main(String[] args) throws IOException {
        if (args.length < 3) {
            System.err.println(
                    "usage: java LoopGen.java <seed> <count> <outdir> [kernels=3] [osrN=50000] [warm=20000] [variants=0]");
            System.exit(2);
        }
        long seed = Long.parseLong(args[0]);
        int count = Integer.parseInt(args[1]);
        Path out = Paths.get(args[2]);
        int kernels = args.length > 3 ? Integer.parseInt(args[3]) : 3;
        int osrN = args.length > 4 ? Integer.parseInt(args[4]) : 50000;
        int warm = args.length > 5 ? Integer.parseInt(args[5]) : 20000;
        int variants = args.length > 6 ? Integer.parseInt(args[6]) : 0;
        if (seed < 0 || count < 1 || kernels < 1 || osrN < 32 || warm < 1 || (variants != 0 && variants != 1)) {
            System.err.println("seed must be >= 0, count / kernels / warm >= 1, osrN >= 32, variants 0 or 1");
            System.exit(2);
        }
        Files.createDirectories(out);
        for (int i = 0; i < count; i++) {
            long s = seed + i;
            String name = (variants == 1 ? "R21FuzzGenV" : "R21FuzzGen") + s;
            String src = new LoopGen(s, name, kernels, osrN, warm, variants == 1).program();
            Files.writeString(out.resolve(name + ".java"), src);
            System.out.println(name);
        }
    }

    private static final String[] INT_CONSTS = {
        "0", "1", "2", "3", "7", "(-1)", "(-2)", "31", "127", "(-128)", "255", "65535",
        "0x55555555", "Integer.MAX_VALUE", "Integer.MIN_VALUE", "(Integer.MAX_VALUE - 1)",
        "1000003", "(-65536)"
    };
    private static final String[] LONG_CONSTS = {
        "0L", "1L", "(-1L)", "0x9E3779B97F4A7C15L", "Long.MAX_VALUE", "Long.MIN_VALUE",
        "(Long.MAX_VALUE - 7L)", "4294967296L", "(-4294967296L)", "0xFFFFFFFFL", "1000000007L"
    };
    /** Upper bound of every inner index loop's trip count; arrays are never shorter than 8. */
    private static final int MAX_INNER = 5;
    private static final int MAX_DEPTH = 2;

    private final long seed;
    private final String name;
    private final int kernels;
    private final int osrN;
    private final int warm;
    /** FUZZTOOL21-2 variants; false draws nothing extra (byte-identical programs). */
    private final boolean variants;
    /** The kernel being generated (variants name its LateK / InitK / mkK). */
    private int curK;
    private long state;
    private final StringBuilder sb = new StringBuilder();
    private int ind;
    /** In-scope int variables whose value is always in [0, a.length). */
    private final List<String> idxVars = new ArrayList<>();
    /** In-scope int variables with arbitrary values. */
    private final List<String> wildInts = new ArrayList<>();
    /** In-scope long loop variables. */
    private final List<String> longVars = new ArrayList<>();
    private String outerVar;
    private int budget;
    private int depth;
    private int uniq;

    private LoopGen(long seed, String name, int kernels, int osrN, int warm, boolean variants) {
        this.seed = seed;
        this.name = name;
        this.kernels = kernels;
        this.osrN = osrN;
        this.warm = warm;
        this.variants = variants;
        long s = seed * 0x9E3779B97F4A7C15L + 0x632BE59BD9B4E019L;
        state = s == 0 ? 1 : s;
        for (int i = 0; i < 8; i++) {
            next();
        }
    }

    // ---- deterministic randomness (xorshift64) ----

    private long next() {
        long s = state;
        s ^= s << 13;
        s ^= s >>> 7;
        s ^= s << 17;
        state = s;
        return s;
    }

    private int rnd(int bound) {
        return (int) Long.remainderUnsigned(next(), bound);
    }

    private boolean pct(int p) {
        return rnd(100) < p;
    }

    private String op(String... ops) {
        return ops[rnd(ops.length)];
    }

    private String pick(List<String> l) {
        return l.get(rnd(l.size()));
    }

    // ---- output ----

    private void line(String s) {
        for (int i = 0; i < ind; i++) {
            sb.append("    ");
        }
        sb.append(s).append('\n');
    }

    // ---- constants and leaves ----

    private String intConst() {
        if (pct(70)) {
            return INT_CONSTS[rnd(INT_CONSTS.length)];
        }
        int v = rnd(2001) - 1000;
        return v < 0 ? "(" + v + ")" : Integer.toString(v);
    }

    private String longConst() {
        if (pct(70)) {
            return LONG_CONSTS[rnd(LONG_CONSTS.length)];
        }
        long v = (long) rnd(2_000_001) - 1_000_000L;
        return v < 0 ? "(" + v + "L)" : v + "L";
    }

    /** An int leaf that reads no array (used inside indices). */
    private String simpleIntLeaf() {
        switch (rnd(7)) {
            case 0: return "x";
            case 1: return "c";
            case 2: return wildInts.isEmpty() ? "n" : pick(wildInts);
            case 3: return "S0";
            case 4: return "o.f0";
            case 5: return idxVars.isEmpty() ? "q" : pick(idxVars);
            default: return intConst();
        }
    }

    /** An index expression always in [0, arr.length) (arrays are of length n >= 8). */
    private String safeIdx(String arr) {
        if (!idxVars.isEmpty() && pct(65)) {
            return pick(idxVars);
        }
        return "(((" + simpleIntLeaf() + ") & 0x7fffffff) % " + arr + ".length)";
    }

    private String intLeaf() {
        switch (rnd(12)) {
            case 0:
            case 1: return "x";
            case 2: return "c";
            case 3:
            case 4: return idxVars.isEmpty() ? "n" : pick(idxVars);
            case 5: return wildInts.isEmpty() ? "p" : pick(wildInts);
            case 6: return intConst();
            case 7: return "S0";
            case 8: return "o.f0";
            case 9: return "a[" + safeIdx("a") + "]";
            case 10: return "((int) b[" + safeIdx("b") + "])";
            default: return pct(50) ? "q" : "S2";
        }
    }

    private String longLeaf() {
        switch (rnd(8)) {
            case 0:
            case 1: return "y";
            case 2: return longVars.isEmpty() ? "S1" : pick(longVars);
            case 3: return longConst();
            case 4: return "S1";
            case 5: return "o.g0";
            case 6: return "b[" + safeIdx("b") + "]";
            default: return pct(50) ? "((long) x)" : "(n * 0x9E37L)";
        }
    }

    // ---- expressions (every non-leaf is parenthesised) ----

    private String intExpr(int d) {
        if (d <= 0 || pct(30)) {
            return intLeaf();
        }
        switch (rnd(14)) {
            case 0:
            case 1:
            case 2:
                return "(" + intExpr(d - 1) + " " + op("+", "-", "*", "^", "&", "|") + " " + intExpr(d - 1) + ")";
            case 3:
                return "(" + intExpr(d - 1) + " " + op("<<", ">>", ">>>") + " " + intExpr(d - 1) + ")";
            case 4:
                return "(" + intExpr(d - 1) + " / (" + intExpr(d - 1) + " | 1))";
            case 5:
                return "(" + intExpr(d - 1) + " % (" + intExpr(d - 1) + " | 1))";
            case 6:
                return "Math.abs(" + intExpr(d - 1) + ")";
            case 7:
                return "Math.min(" + intExpr(d - 1) + ", " + intExpr(d - 1) + ")";
            case 8:
                return "Integer.rotateLeft(" + intExpr(d - 1) + ", " + rnd(32) + ")";
            case 9:
                return "((" + op("byte", "short", "char") + ") " + intExpr(d - 1) + ")";
            case 10:
                return "(" + cond(d - 1) + " ? " + intExpr(d - 1) + " : " + intExpr(d - 1) + ")";
            case 11:
                return "o.v(" + intExpr(d - 1) + ")";
            case 12:
                return "h0(" + intExpr(d - 1) + ", a)";
            default:
                return "((int) " + longExpr(d - 1) + ")";
        }
    }

    private String longExpr(int d) {
        if (d <= 0 || pct(30)) {
            return longLeaf();
        }
        switch (rnd(9)) {
            case 0:
            case 1:
            case 2:
                return "(" + longExpr(d - 1) + " " + op("+", "-", "*", "^", "&", "|") + " " + longExpr(d - 1) + ")";
            case 3:
                return "(" + longExpr(d - 1) + " " + op("<<", ">>", ">>>") + " " + intExpr(d - 1) + ")";
            case 4:
                return "(" + longExpr(d - 1) + " / (" + longExpr(d - 1) + " | 1L))";
            case 5:
                return "Long.rotateRight(" + longExpr(d - 1) + ", " + intExpr(d - 1) + ")";
            case 6:
                return "h1(" + longExpr(d - 1) + ", " + intExpr(d - 1) + ")";
            case 7:
                return "((long) " + intExpr(d - 1) + ")";
            default:
                return "(" + cond(d - 1) + " ? " + longExpr(d - 1) + " : " + longExpr(d - 1) + ")";
        }
    }

    private String cond(int d) {
        switch (rnd(6)) {
            case 0: return "(" + intExpr(d) + " < " + intExpr(d) + ")";
            case 1: return "((" + intExpr(d) + " & 1) == 0)";
            case 2: return "(" + longExpr(d) + " > 0L)";
            case 3: return idxVars.isEmpty() ? "(x != c)" : "(" + pick(idxVars) + " == p)";
            case 4: return "(o instanceof Sub2)";
            default: return "(" + intExpr(d) + " != " + intExpr(d) + ")";
        }
    }

    // ---- statements ----

    private void block(int n) {
        for (int i = 0; i < n && budget > 0; i++) {
            stmt();
        }
    }

    private void stmt() {
        budget--;
        // Variants only: the short-circuit keeps the draw sequence of a
        // variants=0 program unchanged.
        if (variants && pct(30)) {
            variantStmt();
            return;
        }
        int k = rnd(100);
        if (k < 12) {
            line("x = x " + op("+", "-", "^", "*") + " " + intExpr(3) + ";");
        } else if (k < 20) {
            line("y = y " + op("+", "-", "^") + " " + longExpr(3) + ";");
        } else if (k < 30) {
            line("a[" + safeIdx("a") + "] " + op("=", "+=", "^=") + " " + intExpr(3) + ";");
        } else if (k < 36) {
            line("b[" + safeIdx("b") + "] " + op("=", "+=", "^=") + " " + longExpr(2) + ";");
        } else if (k < 42) {
            switch (rnd(5)) {
                case 0: line("S0 += " + intExpr(2) + ";"); break;
                case 1: line("S1 = S1 * 31L + " + longExpr(2) + ";"); break;
                case 2: line("S2 ^= " + intExpr(2) + ";"); break;
                case 3: line("o.f0 = " + intExpr(2) + ";"); break;
                default: line("o.g0 += " + longExpr(2) + ";"); break;
            }
        } else if (k < 50) {
            line("if (" + cond(2) + ") {");
            ind++;
            block(1 + rnd(2));
            ind--;
            if (pct(40)) {
                line("} else {");
                ind++;
                block(1 + rnd(2));
                ind--;
            }
            line("}");
        } else if (k < 57) {
            String div = outerVar != null && pct(50) ? "(" + outerVar + " - p)" : intExpr(2);
            int add = 1 + rnd(97);
            if (pct(70)) {
                line("try { x = x " + op("/", "%") + " " + div + "; } catch (ArithmeticException e) { c += " + add + "; }");
            } else {
                line("try { y = y " + op("/", "%") + " ((long) " + div + "); } catch (ArithmeticException e) { c += " + add + "; }");
            }
        } else if (k < 63) {
            String v = outerVar != null && pct(70) ? outerVar : (idxVars.isEmpty() ? null : pick(idxVars));
            if (v == null) {
                line("x = x + " + intExpr(2) + ";");
                return;
            }
            int add = 1 + rnd(97);
            if (pct(50)) {
                line("try { x += a[" + v + " + q]; } catch (ArrayIndexOutOfBoundsException e) { c += " + add + "; }");
            } else {
                // JLS 15.26.1: the right-hand side is evaluated BEFORE the bounds check.
                line("try { b[" + v + " + q] = " + longExpr(2) + "; } catch (ArrayIndexOutOfBoundsException e) { c += " + add + "; }");
            }
        } else if (k < 67) {
            // Uncaught here: on the rare path this leaves the loop (or the kernel).
            if (outerVar == null) {
                line("x = x - " + intExpr(2) + ";");
            } else if (pct(50)) {
                line("x += a[" + outerVar + " + q];");
            } else {
                line("x = x / (" + outerVar + " - p);");
            }
        } else if (k < 75) {
            switch (rnd(3)) {
                case 0: line("x += o.v(" + intExpr(2) + ");"); break;
                case 1: line("y += h1(" + longExpr(2) + ", " + intExpr(1) + ");"); break;
                default: line("x ^= h0(" + intExpr(2) + ", a);"); break;
            }
        } else if (k < 79) {
            switch (rnd(3)) {
                case 0: line("d = d * 0.75 + " + intExpr(2) + ";"); break;
                case 1: line("d += ((double) " + longExpr(2) + ") / 3.0;"); break;
                default: line("x += (int) d;"); break;
            }
        } else if (k < 92) {
            if (depth < MAX_DEPTH && budget > 3) {
                nestedLoop();
            } else {
                line("x = x ^ " + intExpr(3) + ";");
            }
        } else if (k < 94) {
            line("if (" + cond(1) + ") break;");
        } else {
            line("x = x + " + intExpr(3) + ";");
        }
    }

    /**
     * FUZZTOOL21-2: one statement of the variant kinds (never called with
     * variants off). Uses the kernel preamble's `lt` / `ng` and the program's
     * InitK / h2 / h3 / hx.
     */
    private void variantStmt() {
        String iv = outerVar != null ? outerVar : (idxVars.isEmpty() ? "x" : pick(idxVars));
        int add = 1 + rnd(97);
        switch (rnd(9)) {
            case 0:
                // CHA: only LateK overrides w, and it is loaded mid-loop.
                line("x += o.w(" + intExpr(2) + ");");
                break;
            case 1:
                // A static read of a class initialised mid-loop (behind `lt`).
                line("if (lt) { x += Init" + curK + ".X ^ Init" + curK + ".T[" + intExpr(1) + " & 7]; }");
                break;
            case 2:
                line("y += h1(" + longExpr(2) + ", ng);");
                break;
            case 3:
                line("y " + op("+=", "^=", "=") + " h2(" + (pct(50) ? "ng" : intExpr(2)) + ", " + longExpr(2) + ");");
                break;
            case 4:
                line("x " + op("+=", "^=", "-=") + " h3(" + longExpr(2) + ", " + (pct(50) ? "ng" : intExpr(2)) + ");");
                break;
            case 5:
                line("b[" + safeIdx("b") + "] += h2(ng, " + longExpr(1) + ") ^ (long) h3(y, ng);");
                break;
            case 6:
                line("try { x += hx(" + intExpr(2) + ", ng); } catch (IllegalStateException e) { c += " + add + "; }");
                break;
            case 7:
                // ng + 1 + (iv & 1) is zero on every other iteration when n & 7 == 0.
                line("try { x = x / (ng + 1 + (" + iv + " & 1)); } catch (ArithmeticException e) { c += " + add + "; }");
                break;
            default:
                line("y += (long) ng * " + intExpr(1) + " + (" + longExpr(1) + " >> (ng & 63));");
                break;
        }
    }

    private void nestedLoop() {
        depth++;
        String v = "i" + depth + "_" + uniq++;
        String last = null;
        boolean guarded = pct(25);
        if (guarded) {
            last = "last" + uniq++;
            line("int " + last + " = -1;");
            line("try {");
            ind++;
        }
        int kind = rnd(5);
        List<String> scope;
        switch (kind) {
            case 0: {
                String m = pct(50) ? Integer.toString(1 + rnd(MAX_INNER)) : "((n & 3) + 1)";
                line("for (int " + v + " = 0; " + v + " < " + m + "; " + v + "++) {");
                scope = idxVars;
                break;
            }
            case 1: {
                int m = 1 + rnd(MAX_INNER);
                line("for (int " + v + " = " + (m - 1) + "; " + v + " >= 0; " + v + "--) {");
                scope = idxVars;
                break;
            }
            case 2: {
                // An int IV just below Integer.MAX_VALUE: the exit test is the
                // last value before the overflow.
                int c1 = 2 + rnd(5);
                int c2 = rnd(c1);
                line("for (int " + v + " = Integer.MAX_VALUE - " + c1 + "; " + v + " < Integer.MAX_VALUE - " + c2
                        + "; " + v + "++) {");
                scope = wildInts;
                break;
            }
            case 3: {
                // A long IV. Near Long.MAX_VALUE either the bound wraps (r < trips:
                // a zero-trip loop) or there is room for the last step (r >= trips +
                // step), never a step that wraps the IV itself.
                int trips = 1 + rnd(5);
                int step = 1 + rnd(3);
                String lb;
                switch (rnd(4)) {
                    case 0: {
                        int r = pct(50) ? trips + step + rnd(3) : rnd(trips);
                        lb = "(Long.MAX_VALUE - " + r + "L)";
                        break;
                    }
                    case 1: lb = "(1L << 40)"; break;
                    case 2: lb = "(Long.MIN_VALUE + 3L)"; break;
                    default: lb = "(-5L)"; break;
                }
                line("for (long " + v + " = " + lb + "; " + v + " < " + lb + " + " + trips + "L; " + v + " += " + step
                        + ") {");
                scope = longVars;
                break;
            }
            default: {
                int m = 1 + rnd(MAX_INNER);
                line("int " + v + " = 0;");
                line("while (" + v + " < " + m + ") {");
                scope = idxVars;
                break;
            }
        }
        ind++;
        if (guarded) {
            line(last + " = " + (kind == 3 ? "(int) " + v : v) + ";");
        }
        scope.add(v);
        block(1 + rnd(3));
        scope.remove(scope.size() - 1);
        if (kind == 4) {
            line(v + "++;");
        }
        ind--;
        line("}");
        if (guarded) {
            ind--;
            line("} catch (ArithmeticException | ArrayIndexOutOfBoundsException e) {");
            line("    c += " + last + " * 31 + 1;");
            line("}");
        }
        depth--;
    }

    private void kernel(int k) {
        line("static long k" + k + "(int[] a, long[] b, Base o, int n, int p, int q) {");
        ind++;
        line("int x = " + intConst() + ";");
        line("long y = " + longConst() + ";");
        line("int c = 0;");
        line("double d = " + rnd(100) + ".5;");
        curK = k;
        boolean mkOn = false;
        if (variants) {
            // FUZZTOOL21-2: `lc` reaches 0 at one outer iteration of a long call
            // only; `lt` stays on from there; `ng` is negative at every OSR entry.
            line("int lc = n > 1000 ? (n >> 2) : -1;");
            line("boolean lt = false;");
            line("int ng = -1 - (n & 7);");
            mkOn = pct(75);
        }
        int lo = rnd(3);
        int kind = rnd(4);
        switch (kind) {
            case 0: line("for (int i0 = " + lo + "; i0 < n; i0++) {"); break;
            case 1: line("for (int i0 = " + lo + "; i0 < n; i0 += " + (2 + rnd(2)) + ") {"); break;
            case 2: line("for (int i0 = n - 1; i0 >= " + lo + "; i0--) {"); break;
            default:
                line("int i0 = " + lo + ";");
                line("while (i0 < n) {");
                break;
        }
        ind++;
        if (variants) {
            line("if (--lc == 0) { lt = true;" + (mkOn ? " o = mk" + k + "();" : "") + " }");
        }
        outerVar = "i0";
        idxVars.add("i0");
        budget = 16 + rnd(14);
        block(3 + rnd(4));
        if (kind == 3) {
            line("i0++;");
        }
        idxVars.remove("i0");
        outerVar = null;
        ind--;
        line("}");
        line("return x * 31L + y + c * 1000003L + (long) d + o.f0 + o.g0;");
        ind--;
        line("}");
        line("");
    }

    private String vBody(boolean mutates) {
        String e;
        switch (rnd(4)) {
            case 0: e = "(x * " + (3 + 2 * rnd(50)) + ") ^ (f0 + " + rnd(1000) + ")"; break;
            case 1: e = "(x >>> " + rnd(31) + ") + f0 * " + (1 + rnd(9)); break;
            case 2: e = "x - (f0 << " + rnd(8) + ")"; break;
            default: e = "Integer.rotateLeft(x, " + rnd(32) + ") + f0"; break;
        }
        return (mutates ? "f0 += x; " : "") + "return " + e + ";";
    }

    /**
     * FUZZTOOL21-2: the helpers and the per-kernel late classes of a variants
     * program. LateK is reached only through `Class.forName` (the verifier of
     * this class never loads it) and InitK only through getstatic.
     */
    private void variantDecls() {
        line("static long h2(int k, long y) { return (long) k * " + (3 + 2 * rnd(5000)) + "L + (y >> " + rnd(64)
                + ") + k; }");
        line("static int h3(long y, int k) { return (int) (y >>> " + rnd(64) + ") + k * " + (3 + 2 * rnd(500))
                + " + (int) y; }");
        line("static int hx(int v, int k) {");
        line("    if (((v ^ k) & " + (7 + 8 * rnd(8)) + ") == " + rnd(7) + ") { throw new IllegalStateException(); }");
        line("    return v * " + (3 + 2 * rnd(50)) + " + k;");
        line("}");
        line("");
        for (int k = 0; k < kernels; k++) {
            line("static final class Late" + k + " extends Sub1 {");
            line("    Late" + k + "() { f0 = " + intConst() + "; g0 = " + longConst() + "; }");
            line("    @Override int v(int x) { " + vBody(true) + " }");
            line("    @Override int w(int x) { return (x * " + (3 + 2 * rnd(500)) + ") - f0; }");
            line("}");
            line("static final class Init" + k + " {");
            line("    static final int[] T = new int[8];");
            line("    static int X;");
            line("    static {");
            line("        for (int i = 0; i < T.length; i++) { T[i] = (S0 + i) * " + (3 + 2 * rnd(500)) + "; }");
            line("        X = S0 ^ " + intConst() + ";");
            line("        S2 += " + (1 + rnd(1000)) + ";");
            line("    }");
            line("}");
            line("static Base mk" + k + "() {");
            line("    try {");
            line("        return (Base) Class.forName(\"" + name + "$Late" + k
                    + "\").getDeclaredConstructor().newInstance();");
            line("    } catch (ReflectiveOperationException e) {");
            line("        throw new IllegalStateException(e);");
            line("    }");
            line("}");
            line("");
        }
    }

    private String program() {
        line("// Generated by regression-suite/jit-fuzz/LoopGen.java, seed " + seed
                + " (FUZZ21-6). Deterministic: every JVM tier must print the same fact lines.");
        if (variants) {
            line("// variants=1 (FUZZTOOL21-2): late-loaded LateK, late InitK <clinit>, negative ng, h2/h3/hx.");
        }
        line("public class " + name + " {");
        ind++;
        line("static int S0 = " + intConst() + ";");
        line("static long S1 = " + longConst() + ";");
        line("static int S2;");
        line("");
        line("static class Base {");
        line("    int f0 = " + intConst() + ";");
        line("    long g0 = " + longConst() + ";");
        line("    int v(int x) { " + vBody(false) + " }");
        if (variants) {
            line("    int w(int x) { return x + " + intConst() + "; }");
        }
        line("}");
        line("static class Sub1 extends Base {");
        line("    @Override int v(int x) { " + vBody(false) + " }");
        line("}");
        line("static class Sub2 extends Base {");
        line("    @Override int v(int x) { " + vBody(true) + " }");
        line("}");
        line("");
        line("static int h0(int x, int[] a) { return a[(x & 0x7fffffff) % a.length] ^ (x * " + (3 + 2 * rnd(500))
                + "); }");
        line("static long h1(long y, int k) { return (y ^ (y >>> " + (1 + rnd(40)) + ")) * " + (3 + 2 * rnd(5000))
                + "L + k; }");
        line("");
        if (variants) {
            variantDecls();
        }
        for (int k = 0; k < kernels; k++) {
            kernel(k);
        }
        line("static long run(int k, int[] a, long[] b, Base o, int n, int p, int q) {");
        line("    try {");
        line("        switch (k) {");
        for (int k = 0; k < kernels; k++) {
            line("            case " + k + ": return k" + k + "(a, b, o, n, p, q);");
        }
        line("            default: return 0L;");
        line("        }");
        line("    } catch (RuntimeException e) {");
        line("        return e.getClass().getName().hashCode() * 7919L + 1L;");
        line("    }");
        line("}");
        line("");
        line("static long fold(int[] a, long[] b) {");
        line("    long h = 17L;");
        line("    for (int i = 0; i < a.length; i++) { h = h * 31L + a[i]; }");
        line("    for (int i = 0; i < b.length; i++) { h = h * 31L + b[i]; }");
        line("    return h;");
        line("}");
        line("");
        line("public static void main(String[] args) {");
        ind++;
        int again = Math.max(8, osrN / 4);
        for (int k = 0; k < kernels; k++) {
            int late = 8 + rnd(57);
            line("{");
            line("    int n = " + osrN + "; int[] a = new int[n]; long[] b = new long[n]; Base o = new Sub1();");
            line("    long r = run(" + k + ", a, b, o, n, -1, 0);");
            line("    System.out.println(\"fact k" + k + "-osr \" + r + \" \" + fold(a, b) + \" \" + o.f0);");
            line("}");
            line("{");
            line("    int[] a = new int[8]; long[] b = new long[8]; Base o = new Sub1(); long acc = 0L;");
            line("    for (int it = 0; it < " + warm + "; it++) { acc = acc * 31L + run(" + k + ", a, b, o, 8, -1, 0); }");
            line("    System.out.println(\"fact k" + k + "-warm \" + acc + \" \" + fold(a, b) + \" \" + o.f0);");
            line("}");
            line("{");
            line("    int n = " + late + "; int[] a = new int[n]; long[] b = new long[n]; Base o = new Sub2();");
            line("    long r = run(" + k + ", a, b, o, n, n - 1, 1);");
            line("    System.out.println(\"fact k" + k + "-late \" + r + \" \" + fold(a, b) + \" \" + o.f0);");
            line("}");
            line("{");
            line("    int n = " + again + "; int[] a = new int[n]; long[] b = new long[n]; Base o = new Sub1();");
            line("    long r = run(" + k + ", a, b, o, n, n - 1, 1);");
            line("    System.out.println(\"fact k" + k + "-again \" + r + \" \" + fold(a, b) + \" \" + o.f0);");
            line("}");
            line("{");
            line("    int[] a = new int[16]; long[] b = new long[16]; long acc = 0L; Base s1 = new Sub1(); Base s2 = new Sub2();");
            line("    for (int it = 0; it < 2000; it++) {");
            line("        Base o = (it % 3 == 0) ? s2 : s1;");
            line("        acc = acc * 31L + run(" + k + ", a, b, o, 16, (it % 7 == 0) ? 15 : -1, (it % 11 == 0) ? 1 : 0);");
            line("    }");
            line("    System.out.println(\"fact k" + k + "-mixed \" + acc + \" \" + fold(a, b) + \" \" + s1.f0 + \" \" + s2.f0);");
            line("}");
        }
        line("System.out.println(\"fact statics \" + S0 + \" \" + S1 + \" \" + S2);");
        ind--;
        line("}");
        ind--;
        line("}");
        return sb.toString();
    }
}
