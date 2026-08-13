import java.util.ArrayList;

/** ArrayList.size() in a tight loop, in its own method, for a flat perf profile. */
public final class AlSizeOnly {
    private static ArrayList<Object> AL;
    private static long sink;
    private static void loop(int n) { long s = 0; for (int i = 0; i < n; i++) { s += AL.size(); } sink += s; }
    public static void main(String[] a) {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 2_000_000;
        AL = new ArrayList<>();
        for (int i = 0; i < 64; i++) { AL.add("e" + i); }
        for (int b = 0; b < 40; b++) { loop(n); }
        System.out.println("sink=" + sink);
    }
}
