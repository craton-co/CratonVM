import java.util.HashMap;

public class BoxProbe {
    static Object[] sink = new Object[1024];
    static HashMap<Integer, Integer> m = new HashMap<>();

    static long box(int n) {
        for (int i = 0; i < n; i++) {
            sink[i & 1023] = Integer.valueOf(i + 1000);
        }
        return sink.length;
    }

    static long put(int n) {
        for (int i = 0; i < n; i++) {
            m.put(i & 127, i & 127);
        }
        return m.size();
    }

    static long get(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            Integer v = m.get(i & 127);
            if (v != null) s += v;
        }
        return s;
    }

    static long lput(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            Long a = Long.valueOf((long) (i & 127));
            s += a.longValue();
        }
        return s;
    }

    static Object[] lsink = new Object[1024];

    static long lbox(int n) {
        for (int i = 0; i < n; i++) {
            lsink[i & 1023] = Long.valueOf((long) (i & 127));
        }
        return lsink.length;
    }

    static long bu(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            Integer a = Integer.valueOf(i + 1000);
            s += a.intValue();
        }
        return s;
    }

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "box";
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 10_000_000;
        int rounds = args.length > 2 ? Integer.parseInt(args[2]) : 4;
        for (int i = 0; i < 128; i++) m.put(i, i);
        long chk = 0;
        for (int r = 0; r <= rounds; r++) {
            long t0 = System.nanoTime();
            switch (mode) {
                case "box": chk = box(n); break;
                case "put": chk = put(n); break;
                case "get": chk = get(n); break;
                case "bu":  chk = bu(n);  break;
                case "lput": chk = lput(n); break;
                case "lbox": chk = lbox(n); break;
                default: throw new IllegalArgumentException(mode);
            }
            long ms = (System.nanoTime() - t0) / 1_000_000;
            System.out.println("r" + r + " " + mode + " " + ms + " ms chk=" + chk);
        }
    }
}
