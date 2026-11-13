// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

// j27 wave 4 (lane MISCROWS): HotSpot 25's answers for the rows this lane
// worked in docs/feature-designs/jit-ir-optimizations.md.
//
//   CD16-1  null-argument helpful-NPE lines of the collection constructors and
//           bulk methods (CRATONVM_COMPAT_NULL_ARG_MESSAGE_FROM_BYTECODE, default
//           ON since j27: --compatible's stand-ins ask the JDK method's own
//           bytecode for the line; --real-jdk runs that bytecode).
//   BN17-6 / BN17-7  BigInteger / BigDecimal edge cases (zero, negatives,
//           Long.MIN_VALUE magnitudes, scale, toString vs toPlainString,
//           MathContext rounding, exception messages): pins for whichever road
//           serves them (natives in --compatible, JDK bytecode otherwise).
//
// Each null-argument row runs ROUNDS times so interpreted and compiled callers
// both answer; `bad` counts rounds whose message differed from round 0.
//
//   java -cp <dir> J27MiscRows > hs.txt
//   cratonvm --cp <dir> J27MiscRows | diff - hs.txt          (--real-jdk)
//   cratonvm --compatible --cp <dir> J27MiscRows | diff - hs.txt
//   also with CRATONVM_COMPAT_NULL_ARG_MESSAGE_FROM_BYTECODE=0 (the literals;
//   same output on JDK 25) and CRATONVM_DBG_NULL_ARG_MESSAGE=1 (expect no
//   [NULLARG] line: the bytecode's answer equals every literal on JDK 25).
//
// j27 wave 9 (lane W9EXC): run the HotSpot reference with -XX:-OmitStackTraceInFastThrow.
// Under default `java`, C2's stackless fast throw (null message) makes null-linkedlist-addall `bad=` and null-linkedlist-addall-at's message
// timing-dependent (measured: bad=619 / 671 / 556; addall-at msg=null). CratonVM's fast throw is opt-in, so its
// default answers equal `java -XX:-OmitStackTraceInFastThrow`, which are the lines below.
// Expected HotSpot 25 output:
//   fact null-arraylist-init bad=0 Cannot invoke "java.util.Collection.toArray()" because "c" is null
//   fact null-arraylist-addall bad=0 Cannot invoke "java.util.Collection.toArray()" because "c" is null
//   fact null-vector-addall bad=0 Cannot invoke "java.util.Collection.toArray()" because "c" is null
//   fact null-linkedlist-init bad=0 Cannot invoke "java.util.Collection.toArray()" because "c" is null
//   fact null-linkedlist-addall bad=0 Cannot invoke "java.util.Collection.toArray()" because "c" is null
//   fact null-linkedlist-addall-at bad=0 Cannot invoke "java.util.Collection.toArray()" because "c" is null
//   fact null-linkedlist-toarray bad=0 Cannot read the array length because "a" is null
//   fact null-arraylist-toarray bad=0 Cannot read the array length because "a" is null
//   fact null-arraydeque-init bad=0 Cannot invoke "java.util.Collection.size()" because "c" is null
//   fact null-arraydeque-addall bad=0 Cannot invoke "java.util.Collection.size()" because "c" is null
//   fact null-hashset-init bad=0 Cannot invoke "java.util.Collection.size()" because "c" is null
//   fact null-linkedhashset-init bad=0 Cannot invoke "java.util.Collection.size()" because "c" is null
//   fact null-treemap-init-sorted bad=0 Cannot invoke "java.util.SortedMap.comparator()" because "m" is null
//   fact null-hashset-containsall bad=0 Cannot invoke "java.util.Collection.iterator()" because "c" is null
//   fact null-arraylist-removeall bad=0 null
//   fact bd-min-long -9223372036854775808 precision=19
//   fact bd-min-long-negate 9223372036854775808
//   fact bd-min-long-scaled -92233720368547758.08
//   fact bd-neg-zero 0.00 scale=2 signum=0
//   fact bd-1e3 1E+3 plain=1000
//   fact bd-0e3 0E+3 plain=0
//   fact bd-small-plain 0.000001234
//   fact bd-small-sci 1.234E-7
//   fact bd-1e-7 1E-7
//   fact bd-strip 123.45
//   fact bd-strip-100 1E+2 plain=100
//   fact bd-strip-zero 0
//   fact bd-strip-zero-scaled 0
//   fact bd-precision 3 scale=12
//   fact bd-double-0.1 0.1000000000000000055511151231257827021181583404541015625
//   fact bd-div-nonterm java.lang.ArithmeticException Non-terminating decimal expansion; no exact representable decimal result.
//   fact bd-div-zero java.lang.ArithmeticException Division by zero
//   fact bd-div-undefined java.lang.ArithmeticException Division undefined
//   fact bd-div-exact 0.125 2.5 0.25
//   fact bd-div-mc32 0.3333333
//   fact bd-round-mc 1.2E+4 10
//   fact bd-setscale 2 -3
//   fact bd-setscale-unnecessary java.lang.ArithmeticException Rounding necessary
//   fact bd-tobiginteger -1
//   fact bd-intvalueexact java.lang.ArithmeticException Overflow
//   fact bi-min-long-negate 9223372036854775808 bitLength=63 absBitLength=64
//   fact bi-neg-zero 0 signum=0
//   fact bi-mod-rem 1 -2 shr=-1
//   fact bi-mod-zero java.lang.ArithmeticException BigInteger: modulus not positive
//   fact bi-div-zero java.lang.ArithmeticException BigInteger divide by zero
//   fact bi-longvalueexact java.lang.ArithmeticException BigInteger out of long range
import java.math.BigDecimal;
import java.math.BigInteger;
import java.math.MathContext;
import java.math.RoundingMode;
import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Collection;
import java.util.HashSet;
import java.util.LinkedHashSet;
import java.util.LinkedList;
import java.util.SortedMap;
import java.util.TreeMap;
import java.util.Vector;

public class J27MiscRows {
    static final int ROUNDS = 3000;

    interface Body {
        void run();
    }

    static final String FAST = "<fast-throw>";
    static String lastFull = "";

    static String npeMessage(Body body) {
        try {
            body.run();
            return "no-exception";
        } catch (NullPointerException e) {
            // C2's stackless fast throw (HotSpot only, nondeterministic) is neither a message nor a defect
            return e.getMessage() == null && e.getStackTrace().length == 0 ? FAST : String.valueOf(e.getMessage());
        } catch (RuntimeException e) {
            return e.getClass().getName() + " " + e.getMessage();
        }
    }

    static void nullRow(String name, Body body) {
        String first = npeMessage(body);
        // a sibling row's warm-up can make round 0 already fast-throw on HotSpot: show the previous full message
        if (first.equals(FAST)) {
            first = lastFull;
        }
        lastFull = first;
        int bad = 0;
        for (int i = 1; i < ROUNDS; i++) {
            String m = npeMessage(body);
            if (!m.equals(FAST) && !first.equals(m)) {
                bad++;
            }
        }
        System.out.println("fact " + name + " bad=" + bad + " " + first);
    }

    static String thrown(Body body) {
        try {
            body.run();
            return "no-exception";
        } catch (RuntimeException e) {
            return e.getClass().getName() + " " + e.getMessage();
        }
    }

    static void nullArgRows() {
        Collection<Object> nc = null;
        Object[] na = null;
        SortedMap<Object, Object> nm = null;
        nullRow("null-arraylist-init", () -> new ArrayList<Object>(nc));
        nullRow("null-arraylist-addall", () -> new ArrayList<Object>().addAll(nc));
        nullRow("null-vector-addall", () -> new Vector<Object>().addAll(nc));
        nullRow("null-linkedlist-init", () -> new LinkedList<Object>(nc));
        nullRow("null-linkedlist-addall", () -> new LinkedList<Object>().addAll(nc));
        nullRow("null-linkedlist-addall-at", () -> new LinkedList<Object>().addAll(0, nc));
        nullRow("null-linkedlist-toarray", () -> new LinkedList<Object>().toArray(na));
        nullRow("null-arraylist-toarray", () -> new ArrayList<Object>().toArray(na));
        nullRow("null-arraydeque-init", () -> new ArrayDeque<Object>(nc));
        nullRow("null-arraydeque-addall", () -> new ArrayDeque<Object>().addAll(nc));
        nullRow("null-hashset-init", () -> new HashSet<Object>(nc));
        nullRow("null-linkedhashset-init", () -> new LinkedHashSet<Object>(nc));
        nullRow("null-treemap-init-sorted", () -> new TreeMap<Object, Object>(nm));
        nullRow("null-hashset-containsall", () -> new HashSet<Object>().containsAll(nc));
        nullRow("null-arraylist-removeall", () -> new ArrayList<Object>().removeAll(nc));
    }

    static void fact(String name, Object value) {
        System.out.println("fact " + name + " " + value);
    }

    static void bigDecimalRows() {
        BigDecimal minLong = BigDecimal.valueOf(Long.MIN_VALUE);
        fact("bd-min-long", minLong + " precision=" + minLong.precision());
        fact("bd-min-long-negate", minLong.negate());
        fact("bd-min-long-scaled", BigDecimal.valueOf(Long.MIN_VALUE, 2));
        BigDecimal negZero = new BigDecimal("-0.00");
        fact("bd-neg-zero", negZero + " scale=" + negZero.scale() + " signum=" + negZero.signum());
        BigDecimal e3 = new BigDecimal("1E+3");
        fact("bd-1e3", e3 + " plain=" + e3.toPlainString());
        BigDecimal z3 = new BigDecimal("0E+3");
        fact("bd-0e3", z3 + " plain=" + z3.toPlainString());
        fact("bd-small-plain", new BigDecimal("0.000001234"));
        fact("bd-small-sci", new BigDecimal("0.0000001234"));
        fact("bd-1e-7", new BigDecimal("1E-7"));
        fact("bd-strip", new BigDecimal("123.4500").stripTrailingZeros());
        BigDecimal hundred = new BigDecimal("100").stripTrailingZeros();
        fact("bd-strip-100", hundred + " plain=" + hundred.toPlainString());
        fact("bd-strip-zero", BigDecimal.ZERO.stripTrailingZeros());
        fact("bd-strip-zero-scaled", new BigDecimal("0.00").stripTrailingZeros());
        BigDecimal small = new BigDecimal("1.23E-10");
        fact("bd-precision", small.precision() + " scale=" + small.scale());
        fact("bd-double-0.1", new BigDecimal(0.1));
        fact("bd-div-nonterm", thrown(() -> BigDecimal.ONE.divide(new BigDecimal("3"))));
        fact("bd-div-zero", thrown(() -> BigDecimal.ONE.divide(BigDecimal.ZERO)));
        fact("bd-div-undefined", thrown(() -> BigDecimal.ZERO.divide(BigDecimal.ZERO)));
        fact(
                "bd-div-exact",
                BigDecimal.ONE.divide(new BigDecimal("8"))
                        + " "
                        + new BigDecimal("10").divide(new BigDecimal("4"))
                        + " "
                        + new BigDecimal("1.00").divide(new BigDecimal("4")));
        fact("bd-div-mc32", BigDecimal.ONE.divide(new BigDecimal("3"), MathContext.DECIMAL32));
        MathContext two = new MathContext(2);
        fact(
                "bd-round-mc",
                new BigDecimal("12345").round(two) + " " + new BigDecimal("9.99").round(two));
        fact(
                "bd-setscale",
                new BigDecimal("2.5").setScale(0, RoundingMode.HALF_EVEN)
                        + " "
                        + new BigDecimal("-2.5").setScale(0, RoundingMode.HALF_UP));
        fact(
                "bd-setscale-unnecessary",
                thrown(() -> new BigDecimal("2.5").setScale(0, RoundingMode.UNNECESSARY)));
        fact("bd-tobiginteger", new BigDecimal("-1.5").toBigInteger());
        fact("bd-intvalueexact", thrown(() -> new BigDecimal("2147483648").intValueExact()));
    }

    static void bigIntegerRows() {
        BigInteger minLong = BigInteger.valueOf(Long.MIN_VALUE);
        fact(
                "bi-min-long-negate",
                minLong.negate()
                        + " bitLength="
                        + minLong.bitLength()
                        + " absBitLength="
                        + minLong.abs().bitLength());
        BigInteger negZero = new BigInteger("-0");
        fact("bi-neg-zero", negZero + " signum=" + negZero.signum());
        BigInteger five = BigInteger.valueOf(-5);
        BigInteger three = BigInteger.valueOf(3);
        fact(
                "bi-mod-rem",
                five.mod(three)
                        + " "
                        + five.remainder(three)
                        + " shr="
                        + BigInteger.valueOf(-1).shiftRight(1));
        fact("bi-mod-zero", thrown(() -> BigInteger.ONE.mod(BigInteger.ZERO)));
        fact("bi-div-zero", thrown(() -> BigInteger.ONE.divide(BigInteger.ZERO)));
        fact("bi-longvalueexact", thrown(() -> minLong.negate().longValueExact()));
    }

    public static void main(String[] args) {
        nullArgRows();
        bigDecimalRows();
        bigIntegerRows();
    }
}
