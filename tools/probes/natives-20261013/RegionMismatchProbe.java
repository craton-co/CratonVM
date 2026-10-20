import java.util.*;

/**
 * String.regionMatches / startsWith / endsWith / equals and Arrays.equals/mismatch/compare
 * over Latin-1 and UTF-16 operands, in-range and faulting, hot enough to be JIT-compiled.
 * The output (one line per section: a checksum and the exception rows) must be identical
 * on HotSpot and on CratonVM in every mode.
 */
public class RegionMismatchProbe {
    static String ex(Throwable t) { return t.getClass().getName() + ": " + t.getMessage(); }

    public static void main(String[] a) {
        String[] pool = {"", "a", "jar:", "JAR:", "jar:nested:/x/y.jar/!BOOT-INF/lib/z.jar!/a/B.class",
            "été", "İstanbul", "café 中文 jar:", "😀x", "😀X", "ab\ud800cd", "ab\ud800cD",
            "0123456789abcdef0123456789abcdef", "0123456789abcdef0123456789abcdeg"};
        int[] offs = {-1, 0, 1, 3, 4, 5, 31, 32, Integer.MAX_VALUE, Integer.MIN_VALUE};
        int[] lens = {-1, 0, 1, 2, 4, 8, 31, 32, 33, Integer.MAX_VALUE, Integer.MIN_VALUE};
        for (int round = 0; round < 3; round++) {          // later rounds run compiled code
            long h = 17; StringBuilder faults = new StringBuilder();
            for (int rep = 0; rep < (round == 0 ? 1 : 300); rep++)
            for (String s : pool) for (String o : pool) for (int to : offs) for (int oo : offs) for (int ln : lens) {
                try { h = h * 31 + (s.regionMatches(to, o, oo, ln) ? 1 : 0); } catch (Throwable t) { if (rep == 0) faults.append("rm:").append(ex(t)).append('\n'); h = h * 31 + 7; }
                try { h = h * 31 + (s.regionMatches(true, to, o, oo, ln) ? 1 : 0); } catch (Throwable t) { if (rep == 0) faults.append("rmi:").append(ex(t)).append('\n'); h = h * 31 + 7; }
            }
            System.out.println("regionMatches round " + round + " " + h);
            System.out.println("regionMatches faults " + new TreeSet<>(Arrays.asList(faults.toString().split("\n"))));
            h = 17;
            for (int rep = 0; rep < (round == 0 ? 1 : 2000); rep++)
            for (String s : pool) for (String o : pool) for (int to : offs) {
                h = h * 31 + (s.startsWith(o) ? 1 : 0) + (s.endsWith(o) ? 2 : 0) + (s.equals(o) ? 4 : 0) + (s.startsWith(o, to) ? 8 : 0)
                    + (s.equalsIgnoreCase(o) ? 16 : 0) + Integer.signum(s.compareTo(o)) * 32 + Integer.signum(s.compareToIgnoreCase(o)) * 64;
            }
            System.out.println("starts/ends/equals/compare round " + round + " " + h);
        }
        // Arrays.mismatch / equals / compare over byte[] and char[], ranges and faults
        Random r = new Random(42);
        for (int round = 0; round < 3; round++) {
            long h = 5; TreeSet<String> faults = new TreeSet<>();
            for (int rep = 0; rep < (round == 0 ? 200 : 20000); rep++) {
                int n = r.nextInt(40);
                byte[] x = new byte[n], y = new byte[n + r.nextInt(3)];
                r.nextBytes(x); System.arraycopy(x, 0, y, 0, Math.min(x.length, y.length));
                if (n > 0 && r.nextBoolean()) y[r.nextInt(Math.min(x.length, y.length))] ^= (byte) (1 << r.nextInt(8));
                char[] cx = new char[n], cy = new char[n];
                for (int i = 0; i < n; i++) { cx[i] = (char) r.nextInt(0x3000); cy[i] = cx[i]; }
                if (n > 0 && r.nextBoolean()) cy[r.nextInt(n)] ^= 1 << r.nextInt(12);
                h = h * 31 + Arrays.mismatch(x, y) + Arrays.mismatch(cx, cy) + (Arrays.equals(x, y) ? 1 : 0) + (Arrays.equals(cx, cy) ? 3 : 0)
                    + Integer.signum(Arrays.compare(x, y)) + Integer.signum(Arrays.compare(cx, cy));
                int from = r.nextInt(n + 3) - 1, to = from + r.nextInt(n + 3);
                try { h = h * 31 + Arrays.mismatch(x, from, Math.min(to, x.length + 2), y, Math.max(from, -1), Math.min(to + 1, y.length + 2)); }
                catch (Throwable t) { faults.add(ex(t)); h = h * 31 + 7; }
                try { h = h * 31 + (Arrays.equals(x, from, to, y, from, to) ? 1 : 0); } catch (Throwable t) { faults.add(ex(t)); h = h * 31 + 9; }
            }
            try { Arrays.mismatch((byte[]) null, new byte[1]); } catch (Throwable t) { faults.add("null:" + ex(t)); }
            try { Arrays.equals(new byte[2], 0, 5, new byte[2], 0, 5); } catch (Throwable t) { faults.add("range:" + ex(t)); }
            System.out.println("arrays round " + round + " " + h + " " + faults);
        }
    }
}
