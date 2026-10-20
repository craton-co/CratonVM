import java.util.concurrent.ConcurrentHashMap;

public class ChmProbe {
    static ConcurrentHashMap<Integer, Integer> m = new ConcurrentHashMap<>();
    static ConcurrentHashMap<String, Integer> sm = new ConcurrentHashMap<>();

    static long get(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            Integer v = m.get(i & 1023);
            if (v != null) s += v;
        }
        return s;
    }

    static long sget(int n) {
        long s = 0;
        String[] keys = new String[1024];
        for (int i = 0; i < 1024; i++) keys[i] = "k" + i;
        for (int i = 0; i < n; i++) {
            Integer v = sm.get(keys[i & 1023]);
            if (v != null) s += v;
        }
        return s;
    }

    static long put(int n) {
        for (int i = 0; i < n; i++) m.put(i & 1023, i & 1023);
        return m.size();
    }

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "get";
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 30_000_000;
        int rounds = args.length > 2 ? Integer.parseInt(args[2]) : 3;
        for (int i = 0; i < 1024; i++) { m.put(i, i); sm.put("k" + i, i); }
        long chk = 0;
        for (int r = 0; r <= rounds; r++) {
            long t0 = System.nanoTime();
            switch (mode) {
                case "get": chk = get(n); break;
                case "sget": chk = sget(n); break;
                case "put": chk = put(n); break;
                default: throw new IllegalArgumentException(mode);
            }
            long ms = (System.nanoTime() - t0) / 1_000_000;
            System.out.println("r" + r + " " + mode + " " + ms + " ms chk=" + chk);
        }
    }
}
