import java.math.BigInteger;

/** All fourteen `BigInteger` rows lane 2 held back, re-asked with the JIT
 *  defect fixed.
 *
 *  Lane 2 held these on a RULE, not a list: "a row whose real body can
 *  dereference a null reference ARGUMENT is exposed to the JIT's dropped
 *  NullPointerException message, and which rows show it varies run to run."
 *  That defect was fixed by `0d013f359` (`probes/L2JitNpeProbe.java`,
 *  `vm/tests/jit_npe_message_hot_equals_cold.rs`), and the page that retired the
 *  lane says the re-measurement "is the whole of what is left here". So this
 *  probe covers every row the rule holds, not only the eight whose message was
 *  seen to move -- a rule is lifted by measuring the population, not the sample.
 *
 *  Shape taken from `L2JitNpeProbe`: the cold and the hot call go through the
 *  SAME static method, so warming compiles the very method that then receives
 *  the null. A pair whose two lines differ is the compiler and can be nothing
 *  else.
 *
 *  Three rows are the OTHER blocker -- `add`/`subtract`/`multiply`, where
 *  refusing the bridge under `--jdk-only` wakes a dead `math_bignum.rs`
 *  Intrinsic that answers null for a null argument instead of throwing. They are
 *  in here so both blockers are read off one run: a row answering
 *  `NO THROW: null` is the survivor, not the JIT.
 */
public class L2BigIntNpe14 {
    static final int WARM = 200_000;
    static final BigInteger A = new BigInteger("123456789012345678901234567890");
    static final BigInteger B = BigInteger.valueOf(97);
    static int sink = 0;

    static void row(String label, Object v) {
        System.out.println(label + " |" + v + "|");
    }

    static String msg(Call c) {
        try {
            BigInteger r = c.run();
            return "NO THROW: " + r;
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    interface Call { BigInteger run(); }

    // ---- one method per operation, so each is compiled independently ----
    static BigInteger doAdd(BigInteger a, BigInteger b) { return a.add(b); }
    static BigInteger doSubtract(BigInteger a, BigInteger b) { return a.subtract(b); }
    static BigInteger doMultiply(BigInteger a, BigInteger b) { return a.multiply(b); }
    static BigInteger doRemainder(BigInteger a, BigInteger b) { return a.remainder(b); }
    static BigInteger doMod(BigInteger a, BigInteger b) { return a.mod(b); }
    static BigInteger doGcd(BigInteger a, BigInteger b) { return a.gcd(b); }
    static BigInteger doAnd(BigInteger a, BigInteger b) { return a.and(b); }
    static BigInteger doOr(BigInteger a, BigInteger b) { return a.or(b); }
    static BigInteger doXor(BigInteger a, BigInteger b) { return a.xor(b); }
    static BigInteger doDivide(BigInteger a, BigInteger b) { return a.divide(b); }
    static BigInteger doModInverse(BigInteger a, BigInteger b) { return a.modInverse(b); }
    static BigInteger doModPow(BigInteger a, BigInteger e, BigInteger m) { return a.modPow(e, m); }
    static BigInteger doInitBytes(byte[] v) { return new BigInteger(v); }
    static BigInteger doInitSignumBytes(int s, byte[] v) { return new BigInteger(s, v); }

    interface Op2 { BigInteger apply(BigInteger a, BigInteger b); }
    interface Op3 { BigInteger apply(BigInteger a, BigInteger b, BigInteger c); }

    /** Cold row, then WARM iterations with valid arguments, then the same call
     *  again. `wa`/`wb` are the warm-up arguments because `modInverse` needs a
     *  coprime pair -- warm it with the 30-digit A and 97 and every iteration
     *  throws, the method is never compiled, and the hot row is VACUOUS while
     *  looking like a measurement. A warm-up that dies says so on its own row.
     */
    static void pair2(String name, Op2 op, BigInteger wa, BigInteger wb) {
        row(name + " cold", msg(() -> op.apply(A, null)));
        String warmFailure = null;
        for (int i = 0; i < WARM; i++) {
            try {
                sink += op.apply(wa, wb).signum();
            } catch (Throwable t) {
                warmFailure = t.getClass().getName() + ": " + t.getMessage();
                break;
            }
        }
        if (warmFailure != null) { row(name + " WARM-UP FAILED", warmFailure); return; }
        row(name + " hot ", msg(() -> op.apply(A, null)));
    }

    /** `modPow` takes TWO reference parameters, so it has two null positions and
     *  the JDK does not necessarily reach them in argument order. Both are asked.
     */
    static void pair3(String name, Op3 op, BigInteger wa, BigInteger we, BigInteger wm) {
        row(name + " nullExp cold", msg(() -> op.apply(A, null, B)));
        row(name + " nullMod cold", msg(() -> op.apply(A, B, null)));
        String warmFailure = null;
        for (int i = 0; i < WARM; i++) {
            try {
                sink += op.apply(wa, we, wm).signum();
            } catch (Throwable t) {
                warmFailure = t.getClass().getName() + ": " + t.getMessage();
                break;
            }
        }
        if (warmFailure != null) { row(name + " WARM-UP FAILED", warmFailure); return; }
        row(name + " nullExp hot ", msg(() -> op.apply(A, null, B)));
        row(name + " nullMod hot ", msg(() -> op.apply(A, B, null)));
    }

    /** `new BigInteger(byte[])` with a null array. Separate from `pair2` because
     *  the parameter is not a `BigInteger`.
     */
    static void initBytesPair() {
        byte[] warm = new byte[] { 1, 2, 3, 4 };
        row("init([B)    cold", msg(() -> doInitBytes(null)));
        String warmFailure = null;
        for (int i = 0; i < WARM; i++) {
            try {
                sink += doInitBytes(warm).signum();
            } catch (Throwable t) {
                warmFailure = t.getClass().getName() + ": " + t.getMessage();
                break;
            }
        }
        if (warmFailure != null) { row("init([B)    WARM-UP FAILED", warmFailure); return; }
        row("init([B)    hot ", msg(() -> doInitBytes(null)));
    }

    /** `new BigInteger(int, byte[])` with a null array. */
    static void initSignumBytesPair() {
        byte[] warm = new byte[] { 1, 2, 3, 4 };
        row("init(I[B)   cold", msg(() -> doInitSignumBytes(1, null)));
        String warmFailure = null;
        for (int i = 0; i < WARM; i++) {
            try {
                sink += doInitSignumBytes(1, warm).signum();
            } catch (Throwable t) {
                warmFailure = t.getClass().getName() + ": " + t.getMessage();
                break;
            }
        }
        if (warmFailure != null) { row("init(I[B)   WARM-UP FAILED", warmFailure); return; }
        row("init(I[B)   hot ", msg(() -> doInitSignumBytes(1, null)));
    }

    public static void main(String[] args) {
        // The survivor three.
        pair2("add       ", L2BigIntNpe14::doAdd, A, B);
        pair2("subtract  ", L2BigIntNpe14::doSubtract, A, B);
        pair2("multiply  ", L2BigIntNpe14::doMultiply, A, B);
        // The eight held on the JIT message.
        pair2("remainder ", L2BigIntNpe14::doRemainder, A, B);
        pair2("mod       ", L2BigIntNpe14::doMod, A, B);
        pair2("gcd       ", L2BigIntNpe14::doGcd, A, B);
        pair2("and       ", L2BigIntNpe14::doAnd, A, B);
        pair2("or        ", L2BigIntNpe14::doOr, A, B);
        pair2("xor       ", L2BigIntNpe14::doXor, A, B);
        pair2("divide    ", L2BigIntNpe14::doDivide, A, B);
        // 3 and 7 are coprime, so the inverse exists on every iteration.
        pair2("modInverse", L2BigIntNpe14::doModInverse,
              BigInteger.valueOf(3), BigInteger.valueOf(7));
        // The three held only by the structural rule.
        pair3("modPow    ", L2BigIntNpe14::doModPow,
              BigInteger.valueOf(5), BigInteger.valueOf(3), BigInteger.valueOf(7));
        initBytesPair();
        initSignumBytesPair();
        row("sink", sink);
    }
}
