import java.io.Serializable;
import java.util.function.*;

/**
 * Correctness oracle for the compiled `invokedynamic` bridge.
 *
 * `IndyScopeProbe` prices the indy penalty; this one asks whether bridging it
 * still produces the same answers. Every arm runs enough iterations to be
 * JIT-compiled, so the bridged path is the one under test rather than the
 * interpreter's, and every arm folds its result into one checksum that must
 * match a real JDK exactly.
 *
 * The arms are chosen where a bridge implemented from the descriptor alone is
 * plausibly wrong:
 *
 *   capture-J / capture-D  a category-2 capture. `count_param_slots` counts
 *                          ONE slot per argument and `count_param_slots_jvm_spec`
 *                          counts two; a bridge that used the wrong one would
 *                          pop the wrong number of stack entries and read a
 *                          neighbouring value as the capture.
 *   capture-mixed          object + primitive captures interleaved, so a
 *                          decode that treated every slot as a reference would
 *                          hand the collector an integer as an oop.
 *   ctor-ref / bound-ref   `LambdaMetafactory` shapes that are not a lambda
 *                          body at all.
 *   serializable           `altMetafactory`, whose bootstrap takes extra
 *                          static arguments.
 *   concat-and-lambda      BOTH bridged bootstraps in one method, which is the
 *                          case that proves the site pointer's `kind` tag is
 *                          read rather than assumed.
 *   throwing               an exception raised INSIDE a bridged lambda's body,
 *                          caught by the caller. The bridge's failure edge
 *                          returns the `i64::MIN` deopt sentinel; a compiled
 *                          caller that did not check it would push the
 *                          sentinel bits as an object reference.
 */
public class IndyBridgeProbe {

    interface LongFn { long apply(long x); }

    interface SerFn extends Serializable { int apply(int x); }

    record Box(int v) {
        int doubled() { return v * 2; }
    }

    static long checksum;

    static void fold(long v) { checksum = checksum * 1000003L + v; }

    // --- arms -------------------------------------------------------------
    // Each creates its lambda INSIDE the timed method, which is the shape that
    // used to make the method permanently uncompilable.

    static long captureLong(long cap, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            LongFn f = x -> x + cap;
            s += f.apply(i);
        }
        return s;
    }

    static long captureDouble(double cap, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            DoubleUnaryOperator f = x -> x * cap;
            s += (long) f.applyAsDouble(i);
        }
        return s;
    }

    static long captureMixed(String tag, int a, long b, double c, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            IntSupplier f = () -> tag.length() + a + (int) b + (int) c;
            s += f.getAsInt();
        }
        return s;
    }

    static long ctorRef(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            IntFunction<Box> f = Box::new;
            s += f.apply(i).doubled();
        }
        return s;
    }

    static long boundRef(int n) {
        long s = 0;
        Box b = new Box(7);
        for (int i = 0; i < n; i++) {
            IntSupplier f = b::doubled;
            s += f.getAsInt() + i;
        }
        return s;
    }

    static long serializableLambda(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            SerFn f = x -> x * 3 + 1;
            s += f.apply(i);
        }
        return s;
    }

    static long concatAndLambda(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            IntUnaryOperator f = x -> x ^ 0x5A5A;
            String t = "v=" + f.applyAsInt(i) + ":" + (long) i;
            s += t.length();
        }
        return s;
    }

    static long throwingLambda(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            IntUnaryOperator f = x -> {
                if ((x & 1023) == 7) {
                    throw new IllegalStateException("boom" + x);
                }
                return x + 1;
            };
            try {
                s += f.applyAsInt(i);
            } catch (IllegalStateException e) {
                s += e.getMessage().length();
            }
        }
        return s;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 200000;
        // Warm every arm past the compile threshold, then measure the answers.
        for (int pass = 0; pass < 2; pass++) {
            checksum = 0;
            fold(captureLong(0x0123456789ABCDEFL, n));
            fold(captureDouble(1.5, n));
            fold(captureMixed("abc", 11, 22L, 33.0, n));
            fold(ctorRef(n));
            fold(boundRef(n));
            fold(serializableLambda(n));
            fold(concatAndLambda(n));
            fold(throwingLambda(n));
            System.out.println("pass " + pass + " checksum=" + checksum);
        }
        System.out.flush();
        Runtime.getRuntime().halt(0);
    }
}
