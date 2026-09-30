/**
 * SbParity — every edge the inline StringBuilder.append(char) / length()
 * intrinsics guard on, exercised until they are hot enough to be compiled,
 * then printed. Byte-identical output on HotSpot and CratonVM is the gate.
 *
 * The guards, and the case that crosses each one:
 *   coder == LATIN1        -> a UTF16 builder (append a non-LATIN1 char first)
 *   count  < value.length  -> growth, exactly at and past the boundary
 *   ch    <= 0xFF          -> a char above LATIN1 into a LATIN1 builder
 *   receiver class id      -> StringBuffer (synchronized, has toStringCache)
 *   receiver non-null      -> a null builder
 */
public class SbParity {

    static String hot(int rounds) {
        // The loop that gets compiled: LATIN1, in-capacity, then growing.
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < rounds; i++) {
            sb.append((char) ('a' + (i % 26)));
            if (sb.length() > 512) {
                sb.setLength(0);
            }
        }
        return sb.toString();
    }

    static String boundary(int cap) {
        // Fill exactly to capacity, then one past it.
        StringBuilder sb = new StringBuilder(cap);
        for (int i = 0; i < cap; i++) {
            sb.append('x');
        }
        sb.append('y');
        return sb.length() + ":" + sb.charAt(cap - 1) + sb.charAt(cap);
    }

    static String inflate(int rounds) {
        // A builder that becomes UTF16 on the first append, so every later
        // append(char) fails the coder guard and must take the native.
        StringBuilder sb = new StringBuilder();
        sb.append('中');
        for (int i = 0; i < rounds; i++) {
            sb.append((char) ('a' + (i % 26)));
        }
        return sb.length() + ":" + sb.charAt(0) + ":" + sb.charAt(1)
               + ":" + sb.charAt(sb.length() - 1);
    }

    static String widen(int rounds) {
        // LATIN1 builder, then a char above 0xFF — the array must inflate and
        // every character already in it must survive the widening.
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < rounds; i++) {
            sb.append((char) ('a' + (i % 26)));
        }
        sb.append('ÿ');
        sb.append('中');
        sb.append('z');
        return sb.length() + ":" + sb.charAt(0) + ":" + sb.charAt(rounds)
               + ":" + sb.charAt(rounds + 1) + ":" + sb.charAt(rounds + 2);
    }

    static String buffer(int rounds) {
        // StringBuffer: synchronized, and it caches toString(). The class-id
        // guard must keep it off the inline path entirely.
        StringBuffer sb = new StringBuffer();
        for (int i = 0; i < rounds; i++) {
            sb.append((char) ('a' + (i % 26)));
            if (i % 97 == 0) {
                String s = sb.toString();
                if (s.length() != sb.length()) {
                    return "CACHE-DESYNC";
                }
            }
        }
        return sb.length() + ":" + sb.toString().length() + ":" + sb.charAt(0);
    }

    static String nulls() {
        StringBuilder sb = null;
        try {
            sb.append('x');
            return "no-npe";
        } catch (NullPointerException e) {
            return "npe";
        }
    }

    static String chained(int rounds) {
        // append returns the receiver; the chain must see the same object.
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < rounds; i++) {
            StringBuilder r = sb.append('a').append('b');
            if (r != sb) {
                return "IDENTITY-BROKEN";
            }
        }
        return String.valueOf(sb.length());
    }

    static String mixedWidths(int rounds) {
        StringBuilder sb = new StringBuilder();
        long h = 0;
        for (int i = 0; i < rounds; i++) {
            sb.append((char) (i & 0xFF));
            h = h * 31 + sb.length();
            if (sb.length() > 300) {
                h = h * 31 + sb.charAt(7) + sb.charAt(sb.length() - 1);
                sb.setLength(0);
            }
        }
        return h + ":" + sb.length();
    }

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 200000;

        System.out.println("hot        = " + hot(rounds).length());
        System.out.println("hot.tail   = " + tail(hot(rounds)));
        for (int cap : new int[] {1, 2, 7, 8, 15, 16, 17, 63, 64, 65}) {
            System.out.println("boundary" + cap + " = " + boundary(cap));
        }
        System.out.println("inflate    = " + inflate(rounds / 4));
        System.out.println("widen      = " + widen(300));
        System.out.println("buffer     = " + buffer(rounds / 4));
        System.out.println("nulls      = " + nulls());
        System.out.println("chained    = " + chained(rounds / 4));
        System.out.println("mixed      = " + mixedWidths(rounds));
        // Run the hot loop again after everything else has been compiled.
        System.out.println("hot2.tail  = " + tail(hot(rounds)));
    }

    static String tail(String s) {
        return s.length() <= 24 ? s : s.substring(s.length() - 24);
    }
}
