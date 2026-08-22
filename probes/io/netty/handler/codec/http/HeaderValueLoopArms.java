package io.netty.handler.codec.http;

import io.netty.util.AsciiString;

import java.nio.ByteBuffer;
import java.util.function.Supplier;

import static io.netty.handler.codec.http.HttpHeaderValidationUtil.validateValidHeaderValue;
import static org.junit.jupiter.api.Assertions.assertNotEquals;

/**
 * `HttpHeaderValidationUtilTest`'s value loop with ONE RUNG ADDED PER ARM, so
 * the class's per-iteration cost can be attributed instead of guessed at.
 *
 * The sibling page's `StatusLoopArmsProbe` is the model. What each arm adds:
 *
 *   bare        the loop and nothing else — the control, and the floor.
 *   putInt      + `buffer.putInt(0, i)`. `java/nio/ByteBuffer.putInt(II)` is a
 *               REGISTERED NATIVE and `force_native_over_real_jdk_bytecode`
 *               names it, so this rung is one trip down the native funnel per
 *               iteration and it is also what seals the real @Test method out
 *               of the method-entry JIT (`jit-skip-seal
 *               site=calls-native-shadowed-method`).
 *   validate    + `oldHeaderValueValidationAlgorithm`, which is
 *               `seq.length()` plus four `seq.charAt(i)` plus four
 *               `oldValidationAlgorithmValidateValueChar` — about nine call
 *               frames, and the rung the per-call floor prices.
 *   catchEmpty  + the `try`/`catch` around it with an EMPTY handler. The delta
 *               over `validate` is the THROW PATH alone: unwind, handler
 *               search, and (without compiled local handlers) one OSR round
 *               trip per catch.
 *   full        + the real handler body: two `validateValidHeaderValue` calls
 *               and two `assertNotEquals`.
 *
 * Every arm is a SEPARATE method reached exactly ONCE, because OSR is the only
 * door a `@Test` body has and a method reached twice is method-entry compiled
 * instead — a different tier and a different question. Every arm is also free
 * of `long`/`float`/`double` arithmetic: a single `long` anywhere in a method
 * makes every non-oop operand-stack entry untypeable at a deopt point, which
 * is an artifact-wide OSR veto and would silently make one arm the
 * interpreter's number. The window stride is therefore computed by the caller.
 *
 * Sampling follows the sibling probe's third attempt — contiguous WINDOWS of
 * `i++` with the window starts spread across the 32-bit range — because the
 * real loop is contiguous and its two data-dependent `switch`es are almost
 * perfectly predicted on consecutive inputs. See
 * `HeaderValidationLoopRate`'s class comment for the two ways that go wrong.
 *
 *   cratonvm --java-home &lt;jdk&gt; @common.args \
 *       io.netty.handler.codec.http.HeaderValueLoopArms 2000000
 *
 * Check `CRATONVM_DBG_JITC=1` for `OSR-compile FAILED` on any arm before
 * believing its number: an arm that ran interpreted while its siblings
 * compiled reads as a subset costing more than its superset, which is the
 * tell the sibling page records for `StatusLoopArmsProbe`'s `refcheck`.
 */
public final class HeaderValueLoopArms {

    /** Contiguous values per sampling window. */
    private static final int WINDOW = 65536;

    private static final IllegalArgumentException VALIDATION_EXCEPTION =
            new IllegalArgumentException() {
                private static final long serialVersionUID = 1L;

                @Override
                public synchronized Throwable fillInStackTrace() {
                    return this;
                }
            };

    static int sink;

    // ---- the callee chain, copied from the test verbatim ------------------

    private static CharSequence asCharSequence(final AsciiString value) {
        return new CharSequence() {
            @Override public int length() { return value.length(); }
            @Override public char charAt(int index) { return value.charAt(index); }
            @Override public CharSequence subSequence(int start, int end) {
                return asCharSequence(value.subSequence(start, end));
            }
        };
    }

    private static int oldValidationAlgorithmValidateValueChar(int state, char character) {
        if ((character & ~15) == 0) {
            switch (character) {
                case 0x0: throw VALIDATION_EXCEPTION;
                case 0x0b: throw VALIDATION_EXCEPTION;
                case '\f': throw VALIDATION_EXCEPTION;
                default: break;
            }
        }
        switch (state) {
            case 0:
                switch (character) {
                    case '\r': return 1;
                    case '\n': return 2;
                    default: break;
                }
                break;
            case 1:
                if (character == '\n') {
                    return 2;
                }
                throw VALIDATION_EXCEPTION;
            case 2:
                switch (character) {
                    case '\t':
                    case ' ': return 0;
                    default: throw VALIDATION_EXCEPTION;
                }
            default: break;
        }
        return state;
    }

    private static void oldHeaderValueValidationAlgorithm(CharSequence seq) {
        int state = 0;
        for (int index = 0; index < seq.length(); index++) {
            state = oldValidationAlgorithmValidateValueChar(state, seq.charAt(index));
        }
        if (state != 0) {
            throw VALIDATION_EXCEPTION;
        }
    }

    // ---- the arms ---------------------------------------------------------

    static void bare(int windows, int stride) {
        for (int w = 0; w < windows; w++) {
            int i = Integer.MIN_VALUE + w * stride;
            for (int k = 0; k < WINDOW; k++) {
                sink += i;
                i++;
            }
        }
    }

    static void putIntOnly(ByteBuffer buffer, int windows, int stride) {
        for (int w = 0; w < windows; w++) {
            int i = Integer.MIN_VALUE + w * stride;
            for (int k = 0; k < WINDOW; k++) {
                buffer.putInt(0, i);
                i++;
            }
        }
    }

    static void validateOnly(ByteBuffer buffer, AsciiString s, int windows, int stride) {
        for (int w = 0; w < windows; w++) {
            int i = Integer.MIN_VALUE + w * stride;
            for (int k = 0; k < WINDOW; k++) {
                buffer.putInt(0, i);
                swallow(s);
                i++;
            }
        }
    }

    /**
     * The `try`/`catch` lives HERE rather than in the arm, so `validateOnly`'s
     * own body has no exception table at all: a handler in the timed method
     * is the very thing the next arm exists to price, and having one in both
     * would make the delta zero by construction.
     */
    private static void swallow(AsciiString s) {
        try {
            oldHeaderValueValidationAlgorithm(s);
        } catch (IllegalArgumentException ignore) {
            // deliberately empty
        }
    }

    static void catchEmpty(ByteBuffer buffer, AsciiString s, int windows, int stride) {
        for (int w = 0; w < windows; w++) {
            int i = Integer.MIN_VALUE + w * stride;
            for (int k = 0; k < WINDOW; k++) {
                buffer.putInt(0, i);
                try {
                    oldHeaderValueValidationAlgorithm(s);
                } catch (IllegalArgumentException ignore) {
                    sink++;
                }
                i++;
            }
        }
    }

    static void full(ByteBuffer buffer, AsciiString s, CharSequence cs,
                     Supplier<String> msg, int windows, int stride) {
        for (int w = 0; w < windows; w++) {
            int i = Integer.MIN_VALUE + w * stride;
            for (int k = 0; k < WINDOW; k++) {
                buffer.putInt(0, i);
                try {
                    oldHeaderValueValidationAlgorithm(s);
                } catch (IllegalArgumentException ignore) {
                    assertNotEquals(-1, validateValidHeaderValue(s), msg);
                    assertNotEquals(-1, validateValidHeaderValue(cs), msg);
                }
                i++;
            }
        }
    }

    /** How often the real distribution actually throws, counted rather than assumed. */
    static void countThrows(ByteBuffer buffer, AsciiString s, int windows, int stride) {
        int thrown = 0;
        for (int w = 0; w < windows; w++) {
            int i = Integer.MIN_VALUE + w * stride;
            for (int k = 0; k < WINDOW; k++) {
                buffer.putInt(0, i);
                try {
                    oldHeaderValueValidationAlgorithm(s);
                } catch (IllegalArgumentException ignore) {
                    thrown++;
                }
                i++;
            }
        }
        System.out.printf("throw rate  %8.2f%%  (%d of %d)%n",
                100.0 * thrown / ((long) windows * WINDOW), thrown, (long) windows * WINDOW);
    }

    // ---- driver -----------------------------------------------------------

    /**
     * Warm the two `HttpHeaderValidationUtil` entry points the `full` handler
     * calls. In the real class the 5504 parameterized subtests have made them
     * hot long before the exhaustive method starts; in a bounded probe they
     * are a cold branch the JIT sees late, and pricing them cold swamps
     * everything the probe is for.
     */
    static void prime(int n) {
        byte[] array = new byte[4];
        ByteBuffer buffer = ByteBuffer.wrap(array);
        AsciiString s = new AsciiString(buffer, false);
        CharSequence cs = asCharSequence(s);
        int acc = 0;
        for (int i = 0; i < n; i++) {
            buffer.putInt(0, i);
            acc += validateValidHeaderValue(s);
            acc += validateValidHeaderValue(cs);
        }
        sink += acc;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 2_000_000;
        int windows = Math.max(1, n / WINDOW);
        // The one `long` in the program, kept out of every timed method.
        int stride = (int) (4294967296L / windows);
        long iters = (long) windows * WINDOW;
        prime(Math.min(n, 100_000));

        byte[] array = new byte[4];
        final ByteBuffer buffer = ByteBuffer.wrap(array);
        final AsciiString s = new AsciiString(buffer, false);
        CharSequence cs = asCharSequence(s);
        Supplier<String> msg = new Supplier<String>() {
            @Override public String get() { return "mismatch at " + buffer.getInt(0); }
        };

        long t0 = System.nanoTime();
        bare(windows, stride);
        long t1 = System.nanoTime();
        putIntOnly(buffer, windows, stride);
        long t2 = System.nanoTime();
        validateOnly(buffer, s, windows, stride);
        long t3 = System.nanoTime();
        catchEmpty(buffer, s, windows, stride);
        long t4 = System.nanoTime();
        full(buffer, s, cs, msg, windows, stride);
        long t5 = System.nanoTime();

        report("bare      ", t1 - t0, iters, 0);
        report("putInt    ", t2 - t1, iters, t1 - t0);
        report("validate  ", t3 - t2, iters, t2 - t1);
        report("catchEmpty", t4 - t3, iters, t3 - t2);
        report("full      ", t5 - t4, iters, t4 - t3);
        countThrows(buffer, s, windows, stride);
        System.out.println("sink=" + sink);
    }

    static void report(String name, long ns, long iters, long prevNs) {
        double per = (double) ns / iters;
        double delta = (double) (ns - prevNs) / iters;
        System.out.printf("%s %10.2f ns/iter   (+%9.2f over the rung above)   full 4294967296 = %8.1f s%n",
                name, per, delta, per * 4294967296.0 / 1e9);
    }
}
