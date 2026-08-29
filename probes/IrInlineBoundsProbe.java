/**
 * Standalone reproducer for
 * `ir-inline-turns-an-index-out-of-bounds-into-an-internalerror-20260828`.
 *
 * The shape is netty's, reduced to one file: a three-`baload` static accessor
 * (`io.netty.buffer.HeapByteBufUtil.getUnsignedMedium`, 34 bytes) called from a
 * nine-byte instance method (`UnpooledHeapByteBuf._getUnsignedMedium`, whose
 * whole body is `aload_0; getfield array; iload_1; invokestatic; ireturn`).
 * `CRATONVM_JIT_IR_INLINE=1` splices the callee into the caller, and the
 * out-of-bounds read that should raise `IndexOutOfBoundsException` raised
 * `InternalError: precise deoptimization unavailable ... reason UnreachedCode`
 * instead.
 *
 * Prints one line per case so a diff against HotSpot names the case. Anything
 * other than the expected exception type is a failure, and an `InternalError`
 * is the specific failure this probe exists for.
 */
public final class IrInlineBoundsProbe {

    static final int SIZE = 64;

    /** Byte-for-byte netty's `HeapByteBufUtil.getUnsignedMedium`. */
    static int getUnsignedMedium(byte[] memory, int index) {
        return (memory[index] & 0xff) << 16
             | (memory[index + 1] & 0xff) << 8
             |  memory[index + 2] & 0xff;
    }

    /** Byte-for-byte netty's `UnpooledHeapByteBuf._getUnsignedMedium`. */
    static final class Buf {
        final byte[] array;

        Buf(int n) {
            array = new byte[n];
            for (int i = 0; i < n; i++) {
                array[i] = (byte) (i + 1);
            }
        }

        int _getUnsignedMedium(int index) {
            return getUnsignedMedium(array, index);
        }

        /** The single-`baload` sibling, to separate "any spliced array read"
         *  from "the third one specifically". */
        int _getByte(int index) {
            return getByte(array, index);
        }
    }

    static int getByte(byte[] memory, int index) {
        return memory[index];
    }

    static String describe(String label, Runnable body) {
        try {
            body.run();
            return label + " -> NO EXCEPTION";
        } catch (IndexOutOfBoundsException e) {
            // The type the JVMS mandates and the netty test asserts.
            return label + " -> " + e.getClass().getName();
        } catch (Throwable t) {
            return label + " -> WRONG " + t.getClass().getName();
        }
    }

    public static void main(String[] args) {
        final Buf buf = new Buf(SIZE);

        // Warm both accessors past the compile threshold on IN-BOUNDS indices
        // only, so the compiled body is built without ever having seen the
        // out-of-bounds path — which is the state the netty test reaches too.
        long acc = 0;
        for (int rep = 0; rep < 300_000; rep++) {
            acc += buf._getUnsignedMedium(rep % (SIZE - 3));
            acc += buf._getByte(rep % SIZE);
        }
        if (acc == Long.MIN_VALUE) {
            System.out.println("unreachable, keeps the warm-up live");
        }

        StringBuilder out = new StringBuilder();
        // The netty assertion, `getMediumBoundaryCheck2`: a 3-byte read whose
        // LAST byte is the one past the end.
        out.append(describe("medium.lastBytePastEnd",
                () -> buf._getUnsignedMedium(SIZE - 2))).append('\n');
        // Its neighbours, so a diff says which index moved.
        out.append(describe("medium.startPastEnd",
                () -> buf._getUnsignedMedium(SIZE))).append('\n');
        out.append(describe("medium.negative",
                () -> buf._getUnsignedMedium(-1))).append('\n');
        out.append(describe("medium.lastInBounds",
                () -> buf._getUnsignedMedium(SIZE - 3))).append('\n');
        // One `baload`, same splice shape — is it the third read or any read?
        out.append(describe("byte.pastEnd", () -> buf._getByte(SIZE))).append('\n');
        out.append(describe("byte.negative", () -> buf._getByte(-1))).append('\n');
        out.append(describe("byte.lastInBounds", () -> buf._getByte(SIZE - 1))).append('\n');

        System.out.print(out);
    }
}
