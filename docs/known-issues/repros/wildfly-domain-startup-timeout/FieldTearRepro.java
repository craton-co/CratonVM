public class FieldTearRepro {
    static volatile boolean stop = false;
    static long writerIters = 0;
    static long seenA = 0, seenB = 0, bad = 0;
    static int firstBad = 0;

    static final class Holder {
        int x;
    }

    public static void main(String[] args) throws Exception {
        final long seconds = args.length > 0 ? Long.parseLong(args[0]) : 15L;
        final Holder h = new Holder();
        final int A = 0x11111111;
        final int B = 0x22222222;

        Thread writer = new Thread(() -> {
            long i = 0;
            while (!stop) {
                h.x = (i % 2 == 0) ? A : B;
                i++;
            }
            writerIters = i;
        }, "writer");

        Thread reader = new Thread(() -> {
            long a = 0, b = 0, d = 0;
            int fb = 0;
            while (!stop) {
                int v = h.x;
                if (v == A) {
                    a++;
                } else if (v == B) {
                    b++;
                } else {
                    if (d == 0) {
                        fb = v;
                    }
                    d++;
                }
            }
            seenA = a;
            seenB = b;
            bad = d;
            firstBad = fb;
        }, "reader");

        writer.start();
        reader.start();
        Thread.sleep(seconds * 1000L);
        stop = true;
        writer.join();
        reader.join();

        System.out.println("RESULT writerIters=" + writerIters
                + " seenA=" + seenA + " seenB=" + seenB + " bad=" + bad
                + " firstBad=0x" + Integer.toHexString(firstBad));
        System.out.println("done");
    }
}
