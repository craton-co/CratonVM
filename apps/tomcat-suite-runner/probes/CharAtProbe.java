/** Is the JIT's StringCharAt intrinsic actually firing? */
public class CharAtProbe {

    static final int N = 20_000_000;
    static String MSG;
    static int sink;

    static int loopCharAt() {
        int acc = 0;
        String m = MSG;
        for (int i = 0; i < N; i++) {
            acc += m.charAt(i & 1023);
        }
        return acc;
    }

    static int loopLength() {
        int acc = 0;
        String m = MSG;
        for (int i = 0; i < N; i++) {
            acc += m.length();
        }
        return acc;
    }

    static int loopArray() {
        int acc = 0;
        char[] a = new char[1024];
        for (int i = 0; i < N; i++) {
            acc += a[i & 1023];
        }
        return acc;
    }

    static void bench(String label, java.util.function.IntSupplier s) {
        s.getAsInt();
        long best = Long.MAX_VALUE;
        for (int i = 0; i < 3; i++) {
            long t = System.nanoTime();
            sink += s.getAsInt();
            long d = System.nanoTime() - t;
            if (d < best) {
                best = d;
            }
        }
        System.out.printf("%-20s %7.1f ns/op%n", label, Double.valueOf((double) best / N));
    }

    public static void main(String[] args) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 1024; i++) {
            sb.append('x');
        }
        MSG = sb.toString();
        bench("char[] load", CharAtProbe::loopArray);
        bench("String.length()", CharAtProbe::loopLength);
        bench("String.charAt(i)", CharAtProbe::loopCharAt);
        System.out.println("sink=" + sink);
    }
}
