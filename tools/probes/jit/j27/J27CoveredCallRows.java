// Covered nested calls: inherited handlers, guarded hit/miss, and no replay.
// Compare complete stdout with HotSpot 25 after warming the same callers.
public class J27CoveredCallRows {
    interface Worker { int run(int v); }
    static final class Add implements Worker {
        public int run(int v) {
            if (v == -1) throw new IllegalArgumentException("negative");
            if (v == -2) throw new AssertionError("error");
            return v + 1;
        }
    }
    static final class Other implements Worker {
        public int run(int v) {
            if (v == -1) throw new IllegalStateException("other");
            if (v == -2) throw new LinkageError("other-error");
            return v + 2;
        }
    }
    static final class FaultDecoder extends java.nio.charset.CharsetDecoder {
        FaultDecoder() { super(java.nio.charset.StandardCharsets.ISO_8859_1, 1, 1); }
        protected java.nio.charset.CoderResult decodeLoop(java.nio.ByteBuffer in,
                java.nio.CharBuffer out) {
            int value = in.get();
            if (value == -1) throw new IllegalArgumentException("decoder-runtime");
            if (value == -2) throw new AssertionError("decoder-error");
            out.put((char) (value & 255));
            return java.nio.charset.CoderResult.UNDERFLOW;
        }
    }
    static int decodeCall(java.nio.charset.CharsetDecoder d, byte value) {
        java.nio.ByteBuffer in = java.nio.ByteBuffer.wrap(new byte[] {value});
        java.nio.CharBuffer out = java.nio.CharBuffer.allocate(1);
        d.reset();
        d.decode(in, out, true);
        return out.position();
    }
    static void decoderCases() {
        FaultDecoder d = new FaultDecoder();
        long sum = 0;
        for (int i = 0; i < 300000; i++) sum += decodeCall(d, (byte) (i & 127));
        System.out.println("coder-warm " + sum);
        for (int value = -1; value >= -2; value--) {
            try { decodeCall(d, (byte) value); System.out.println("coder-missing"); }
            catch (Throwable e) {
                Throwable cause = e.getCause();
                System.out.println("coder-throw " + e.getClass().getName() + " " + e.getMessage()
                    + " cause=" + (cause == null ? "none"
                        : cause.getClass().getName() + ":" + cause.getMessage()));
            }
        }
    }
    static int effects;
    static volatile long sink;
    static int leaf(int v) { effects++; return v; }
    static int middle(Worker w, int v) { int changed = v + 3; return deeper(w, changed - 3); }
    static int deeper(Worker w, int v) { return w.run(leaf(v)); }
    static int wrapped(Worker w, int v) {
        try { return middle(w, v); }
        catch (RuntimeException e) { throw new IllegalArgumentException("wrapped", e); }
        catch (Error e) { throw new AssertionError("wrapped-error", e); }
    }
    static long row(Worker w, int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) sum += wrapped(w, i & 255);
        return sum;
    }
    static void thrown(Worker w, int v) {
        int before = effects;
        try { sink = rowThrow(w, v); System.out.println("missing"); }
        catch (Throwable e) {
            Throwable cause = e.getCause();
            System.out.println("throw " + e.getClass().getName() + " " + e.getMessage()
                + " cause=" + cause.getClass().getName() + ":" + cause.getMessage()
                + " effects=" + (effects - before));
            for (StackTraceElement f : cause.getStackTrace()) {
                if (f.getClassName().startsWith("J27CoveredCallRows"))
                    System.out.println("frame " + f.getClassName() + "." + f.getMethodName()
                        + ":" + f.getLineNumber());
            }
        }
    }
    static int rowThrow(Worker w, int v) { return wrapped(w, v); }
    static int wrapCall(byte[] array, int offset, int length) {
        return java.nio.ByteBuffer.wrap(array, offset, length).remaining();
    }
    static void wrapCases() {
        byte[] array = new byte[16];
        long sum = 0;
        for (int i = 0; i < 1000000; i++) sum += wrapCall(array, i & 7, 8);
        System.out.println("wrap-warm " + sum);
        for (int which = 0; which < 4; which++) {
            try {
                int v = switch (which) {
                    case 0 -> wrapCall(null, 0, 1);
                    case 1 -> wrapCall(array, -1, 2);
                    case 2 -> wrapCall(array, 0, -1);
                    default -> wrapCall(array, 7, Integer.MAX_VALUE);
                };
                System.out.println("wrap-missing " + which + " " + v);
            } catch (Throwable e) {
                System.out.println("wrap-throw " + which + " " + e.getClass().getName()
                    + " " + e.getMessage());
            }
        }
    }
    public static void main(String[] args) {
        decoderCases();
        wrapCases();
        Worker add = new Add(), other = new Other();
        long sum = 0;
        for (int r = 0; r < 6; r++) sum += row(add, 200000);
        System.out.println("warm " + sum + " effects=" + effects);
        // The same hot caller sees both a profiled receiver and a new receiver.
        for (int r = 0; r < 2000; r++) sink = rowThrow(add, r & 255);
        thrown(add, -1);
        thrown(add, -2);
        for (int r = 0; r < 2000; r++) sink = rowThrow(other, r & 255);
        thrown(other, -1);
        thrown(other, -2);
        System.out.println("other " + row(other, 200000));
    }
}
