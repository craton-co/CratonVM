/**
 * Is the per-operation cost of the TestMethodPerformance hot loop constant, or
 * does it grow with the number of objects already allocated?
 *
 * Measured on CratonVM (2026-07-31): the `mb.setBytes(); mb.toStringType();`
 * loop costs 15.5 us/iter at 100k iterations and 35.3 us/iter at 800k across
 * separate processes, with `-Xlog:gc*=info` reporting NO collections at all.
 * A cost that grows with allocation count but is not GC is the interesting
 * part; the absolute figure is not.
 *
 * Each shape is a plain static method with the loop INLINE -- no lambda, no
 * functional interface. An earlier version drove each shape through an
 * `IntConsumer` and measured ~3.4 us per `accept` call on CratonVM, which
 * buried every shape it was supposed to compare.
 *
 * A flat row means constant cost; a rising row means the operation degrades as
 * the heap fills.
 */
public final class AllocScalingProbe {

    private static Object refSink;
    private static long sink;

    static final class Small {
        int a;
        Small(int a) { this.a = a; }
    }

    private static long newSmallObject(int n) {
        for (int i = 0; i < n; i++) {
            refSink = new Small(i);
        }
        return n;
    }

    private static long newIntArray8(int n) {
        for (int i = 0; i < n; i++) {
            refSink = new int[8];
        }
        return n;
    }

    private static long newCharArray3(int n) {
        for (int i = 0; i < n; i++) {
            refSink = new char[3];
        }
        return n;
    }

    private static long newStringFromChars(int n) {
        char[] c = {'G', 'E', 'T'};
        for (int i = 0; i < n; i++) {
            refSink = new String(c, 0, 3);
        }
        return n;
    }

    private static long arithOnly(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += i ^ (i << 1);
        }
        sink += s;
        return n;
    }

    private static void header(int blocks) {
        System.out.printf("%-22s", "shape (ns/op by block)");
        for (int b = 0; b < blocks; b++) {
            System.out.printf("%8d", b);
        }
        System.out.println();
    }

    private static void row(String name, int blocks, int blockSize, int which) {
        StringBuilder sb = new StringBuilder(String.format("%-22s", name));
        for (int b = 0; b < blocks; b++) {
            long t0 = System.nanoTime();
            switch (which) {
                case 0: newSmallObject(blockSize); break;
                case 1: newIntArray8(blockSize); break;
                case 2: newCharArray3(blockSize); break;
                case 3: newStringFromChars(blockSize); break;
                default: arithOnly(blockSize); break;
            }
            sb.append(String.format("%8d", (System.nanoTime() - t0) / blockSize));
        }
        System.out.println(sb);
    }

    public static void main(String[] args) {
        int blocks = args.length > 0 ? Integer.parseInt(args[0]) : 8;
        int blockSize = args.length > 1 ? Integer.parseInt(args[1]) : 100_000;

        header(blocks);
        row("newSmallObject", blocks, blockSize, 0);
        row("newIntArray8", blocks, blockSize, 1);
        row("newCharArray3", blocks, blockSize, 2);
        row("newStringFromChars", blocks, blockSize, 3);
        row("arithOnly", blocks, blockSize, 4);

        System.out.println("sink=" + sink + " refSink="
                + (refSink == null ? "null" : refSink.getClass().getName()));
        Runtime rt = Runtime.getRuntime();
        System.out.println("heap used=" + (rt.totalMemory() - rt.freeMemory())
                + " total=" + rt.totalMemory());
    }
}
