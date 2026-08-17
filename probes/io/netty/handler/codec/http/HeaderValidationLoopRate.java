package io.netty.handler.codec.http;

import io.netty.util.AsciiString;

import java.nio.ByteBuffer;
import java.util.function.Supplier;

import static io.netty.handler.codec.http.HttpHeaderValidationUtil.validateToken;
import static io.netty.handler.codec.http.HttpHeaderValidationUtil.validateValidHeaderValue;
import static org.junit.jupiter.api.Assertions.assertNotEquals;

/**
 * `HttpHeaderValidationUtilTest`'s two exhaustive loops, verbatim except that
 * each takes `n` SAMPLES spread across the whole 32-bit range instead of every
 * value, and each is reached exactly ONCE — so OSR is the only route out of the
 * interpreter, the shape a `@Test` method has.
 *
 * HOW IT SAMPLES, and why neither obvious way works. Calibrated against the
 * real methods under HotSpot 25 on this host, which take 54.8 s and 32.6 s —
 * 12.8 and 7.6 ns/iter:
 *
 *  * `i++` from `Integer.MIN_VALUE`, bounded: 75.4 ns/iter. That prefix is all
 *    values whose top byte is `0x80`... but the giveaway is the ~5% of inputs
 *    containing `0x00`/`0x0b`/`0x0c`, which THROW and then run two `validateXxx`
 *    calls in the catch. A prefix window can sit entirely inside them.
 *  * a fixed stride across the whole range: 85.1 ns/iter, and NOT because the
 *    throw rate is wrong — it is the branch predictor. The old algorithm is two
 *    data-dependent `switch`es per character over four characters; consecutive
 *    values (`i++`) make those branches near-perfectly predicted, and a strided
 *    walk makes them random. The real loop is contiguous, so a strided probe
 *    measures a workload the class does not have.
 *
 * So: contiguous WINDOWS of `i++`, with the window starts spread across the
 * range. That keeps the branch behaviour of the real loop and still averages
 * over the ~3.5%% of windows whose fixed upper bytes make every iteration throw.
 *
 * Lives in `io.netty.handler.codec.http` so it can reach the same
 * package-private `HttpHeaderValidationUtil` entry points the test does; the two
 * `oldXxxValidationAlgorithm` helpers are private to the test class and are
 * copied here byte-for-byte.
 *
 *   cratonvm --java-home <jdk> @common.args \
 *       io.netty.handler.codec.http.HeaderValidationLoopRate 20000000
 *
 * See docs/known-issues/netty/httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md.
 */
public final class HeaderValidationLoopRate {

    /** Contiguous values per sampling window. See the class comment. */
    private static final int WINDOW = 65536;

    private static final IllegalArgumentException VALIDATION_EXCEPTION = new IllegalArgumentException() {
        private static final long serialVersionUID = -8857428534361331089L;

        @Override
        public synchronized Throwable fillInStackTrace() {
            return this;
        }
    };

    private static CharSequence asCharSequence(final AsciiString value) {
        return new CharSequence() {
            @Override
            public int length() {
                return value.length();
            }

            @Override
            public char charAt(int index) {
                return value.charAt(index);
            }

            @Override
            public CharSequence subSequence(int start, int end) {
                return asCharSequence(value.subSequence(start, end));
            }
        };
    }

    private static int oldValidationAlgorithmValidateValueChar(int state, char character) {
        if ((character & ~15) == 0) {
            switch (character) {
                case 0x0:
                    throw VALIDATION_EXCEPTION;
                case 0x0b:
                    throw VALIDATION_EXCEPTION;
                case '\f':
                    throw VALIDATION_EXCEPTION;
                default:
                    break;
            }
        }
        switch (state) {
            case 0:
                switch (character) {
                    case '\r':
                        return 1;
                    case '\n':
                        return 2;
                    default:
                        break;
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
                    case ' ':
                        return 0;
                    default:
                        throw VALIDATION_EXCEPTION;
                }
            default:
                break;
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

    private static void validateHeaderNameElement(byte value) {
        switch (value) {
            case 0x1c:
            case 0x1d:
            case 0x1e:
            case 0x1f:
            case 0x00:
            case '\t':
            case '\n':
            case 0x0b:
            case '\f':
            case '\r':
            case ' ':
            case ',':
            case ':':
            case ';':
            case '=':
                throw VALIDATION_EXCEPTION;
            default:
                if (value < 0) {
                    throw VALIDATION_EXCEPTION;
                }
        }
    }

    private static void oldHeaderNameValidationAlgorithmAsciiString(AsciiString name) {
        byte[] array = name.array();
        for (int i = name.arrayOffset(), len = name.arrayOffset() + name.length(); i < len; i++) {
            validateHeaderNameElement(array[i]);
        }
    }

    static void headerValueLoop(int n) {
        byte[] array = new byte[4];
        final ByteBuffer buffer = ByteBuffer.wrap(array);
        final AsciiString asciiString = new AsciiString(buffer, false);
        CharSequence charSequence = asCharSequence(asciiString);
        Supplier<String> failureMessageSupplier = new Supplier<String>() {
            @Override
            public String get() {
                return "validation mismatch on string '" + asciiString + "', iteration " + buffer.getInt(0);
            }
        };
        int windows = Math.max(1, n / WINDOW);
        int windowStride = (int) (4294967296L / windows);
        for (int w = 0; w < windows; w++) {
            int i = Integer.MIN_VALUE + w * windowStride;
            for (int k = 0; k < WINDOW; k++) {
                buffer.putInt(0, i);
                try {
                    oldHeaderValueValidationAlgorithm(asciiString);
                } catch (IllegalArgumentException ignore) {
                    assertNotEquals(-1, validateValidHeaderValue(asciiString), failureMessageSupplier);
                    assertNotEquals(-1, validateValidHeaderValue(charSequence), failureMessageSupplier);
                }
                i++;
            }
        }
    }

    static void headerNameLoop(int n) {
        byte[] array = new byte[4];
        final ByteBuffer buffer = ByteBuffer.wrap(array);
        final AsciiString asciiString = new AsciiString(buffer, false);
        CharSequence charSequence = asCharSequence(asciiString);
        Supplier<String> failureMessageSupplier = new Supplier<String>() {
            @Override
            public String get() {
                return "validation mismatch on string '" + asciiString + "', iteration " + buffer.getInt(0);
            }
        };
        int windows = Math.max(1, n / WINDOW);
        int windowStride = (int) (4294967296L / windows);
        for (int w = 0; w < windows; w++) {
            int i = Integer.MIN_VALUE + w * windowStride;
            for (int k = 0; k < WINDOW; k++) {
                buffer.putInt(0, i);
                try {
                    oldHeaderNameValidationAlgorithmAsciiString(asciiString);
                } catch (IllegalArgumentException ignore) {
                    assertNotEquals(-1, validateToken(asciiString), failureMessageSupplier);
                    assertNotEquals(-1, validateToken(charSequence), failureMessageSupplier);
                }
                i++;
            }
        }
    }

    /**
     * Exercise the two `HttpHeaderValidationUtil` entry points the CATCH arm
     * calls, before either loop runs.
     *
     * Not cosmetic. The catch arm runs on ~7.7%% of iterations, so in a bounded
     * probe it is a cold branch that the JIT sees late; in the real class the
     * 5504 parameterized subtests have already made both entry points hot before
     * the exhaustive method starts. Without this the probe prices the catch arm
     * at ~820 ns against the ~74 ns the real method pays on the same VM, which
     * swamps everything the probe exists to measure.
     */
    static void primeValidators(int n) {
        byte[] array = new byte[4];
        ByteBuffer buffer = ByteBuffer.wrap(array);
        AsciiString asciiString = new AsciiString(buffer, false);
        CharSequence charSequence = asCharSequence(asciiString);
        long sink = 0;
        for (int i = 0; i < n; i++) {
            buffer.putInt(0, i);
            sink += validateValidHeaderValue(asciiString);
            sink += validateValidHeaderValue(charSequence);
            sink += validateToken(asciiString);
            sink += validateToken(charSequence);
        }
        if (sink == Long.MIN_VALUE) {
            throw new IllegalStateException();
        }
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 20_000_000;
        // 100 000 is enough for both VMs to compile the two entry points and
        // small enough that the priming itself is not the run: CratonVM is ~1 us
        // per priming iteration before it warms, so a 2 000 000-iteration prime
        // took longer than the measurement it was there to enable.
        primeValidators(Math.min(n, 100_000));
        long iters = (long) Math.max(1, n / WINDOW) * WINDOW;
        long v0 = System.nanoTime();
        headerValueLoop(n);
        long v1 = System.nanoTime();
        headerNameLoop(n);
        long v2 = System.nanoTime();
        double vns = (double) (v1 - v0) / iters;
        double nns = (double) (v2 - v1) / iters;
        System.out.printf("value-loop %8.2f ns/iter => full 4294967296 = %.1f s%n", vns, vns * 4294967296.0 / 1e9);
        System.out.printf("name-loop  %8.2f ns/iter => full 4294967296 = %.1f s%n", nns, nns * 4294967296.0 / 1e9);
        System.out.printf("class total (both loops + 5504 quick tests) => %.1f s%n",
                (vns + nns) * 4294967296.0 / 1e9);
    }
}
