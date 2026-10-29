// JIT round 23 wave 2, lane L3: an NPE raised by a null receiver's field read
// inside a lambda impl's own try/catch, once the impl's optimizing body has
// superseded its first one and the SAM call goes through the compiled direct
// lambda arm. The NPE escaped the impl's catch (page
// docs/feature-designs/jdk-only-mode.md).
public class LambdaImplNpeHandler {
    static class Holder { int x; }
    static Holder NULL_HOLDER;
    static volatile int sink;

    interface Loop { int run(int n); }

    static int loopNpe(int n) {
        int caught = 0;
        for (int i = 0; i < n; i++) {
            try {
                sink += NULL_HOLDER.x;
            } catch (NullPointerException e) {
                caught++;
            }
        }
        return caught;
    }

    static void time(int rep, int n, Loop loop) {
        int c = loop.run(n);
        System.out.println("rep=" + rep + " caught=" + (c == n));
    }

    public static void main(String[] a) {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 200000;
        // Hot first, so main runs the reps below in its OSR body.
        for (int i = 0; i < 300000; i++) {
            sink += i;
        }
        for (int rep = 0; rep < 10; rep++) {
            time(rep, n, LambdaImplNpeHandler::loopNpe);
        }
        System.out.println("done");
    }
}
