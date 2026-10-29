import java.util.Objects;

// R18 wave 9, lane osrframe: a static helper that calls Objects.checkIndex,
// warmed in an OSR'd loop of main (line 26), then called once with a bad
// index (line 32). HotSpot: checkIndex / chk:10 / main:32. Keep the lines.
public class R18OsrFrameCheckIndex {
    static int sink;

    static int chk(int i, int n) {
        return Objects.checkIndex(i, n);
    }

    static void print(String tag, StackTraceElement[] st) {
        StringBuilder sb = new StringBuilder(tag);
        for (StackTraceElement e : st) {
            // All frames, JDK ones included: HotSpot names Objects.java:365.
            sb.append(' ').append(e.getClassName()).append('.').append(e.getMethodName())
              .append(':').append(e.getLineNumber());
        }
        System.out.println(sb);
    }

    public static void main(String[] args) {
        int s = 0;
        for (int i = 0; i < 200_000; i++) {
            s += chk(i & 7, 8);
        }
        sink = s;
        System.out.println("warm " + s);
        int bad = 9;
        try {
            sink = chk(bad, 8);
            System.out.println("no throw");
        } catch (IndexOutOfBoundsException e) {
            System.out.println("caught " + e.getClass().getName() + " " + e.getMessage());
            print("trace", e.getStackTrace());
        }
        second();
        third();
    }

    // A non-OSR shape: a method-entry compiled body with chk inlined.
    static int callChk(int i, int n) {
        return chk(i, n);
    }

    static void second() {
        int s = 0;
        for (int r = 0; r < 20_000; r++) {
            s += callChk(r & 3, 4);
        }
        sink = s;
        try {
            sink = callChk(-1, 4);
            System.out.println("no throw 2");
        } catch (IndexOutOfBoundsException e) {
            print("trace2", e.getStackTrace());
        }
    }

    // getStackTrace / StackWalker reached from a small callee (peek) that the
    // OSR body of third() can splice, and Thread.currentThread() (a thin
    // helper in --compatible) in tail position of a compiled method.
    static Thread cur() {
        return Thread.currentThread();
    }

    static void report() {
        StackTraceElement[] st = cur().getStackTrace();
        print("gst", java.util.Arrays.copyOfRange(st, 1, st.length));
        StringBuilder sb = new StringBuilder("walker");
        StackWalker.getInstance().forEach(f ->
            sb.append(' ').append(f.getClassName()).append('.').append(f.getMethodName())
              .append(':').append(f.getLineNumber()));
        System.out.println(sb);
    }

    static int peek(int i) {
        if (i == 123_456) {
            report();
        }
        return i & 1;
    }

    static void third() {
        int s = 0;
        for (int i = 0; i < 200_000; i++) {
            s += peek(i);
        }
        sink = s;
        System.out.println("third " + s);
    }
}
