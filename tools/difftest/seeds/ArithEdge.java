// difftest: strict
//
// Arithmetic edge cases — the cheap, high-yield family (design §4.6).
// Overflow wrap, Integer.MIN_VALUE / -1, modulo of negatives, and shift-
// distance masking are all places where a JIT or interpreter bug changes a
// printed value. Output is fully deterministic.
public class ArithEdge {
    public static void main(String[] args) {
        int min = Integer.MIN_VALUE;
        long lmin = Long.MIN_VALUE;

        // Overflow wrap-around (two's complement, defined in the JLS).
        System.out.println("int+1 ovf: " + (Integer.MAX_VALUE + 1));
        System.out.println("long+1 ovf: " + (Long.MAX_VALUE + 1L));

        // MIN_VALUE / -1 overflows back to MIN_VALUE (no exception).
        System.out.println("imin/-1: " + (min / -1));
        System.out.println("lmin/-1: " + (lmin / -1L));
        System.out.println("imin%-1: " + (min % -1));

        // Modulo of negatives keeps the sign of the dividend.
        System.out.println("-7%3: " + (-7 % 3));
        System.out.println("7%-3: " + (7 % -3));

        // Shift distance is masked to the low 5 bits (int) / 6 bits (long).
        System.out.println("1<<32: " + (1 << 32));
        System.out.println("1<<33: " + (1 << 33));
        System.out.println("1L<<64: " + (1L << 64));
        System.out.println("-1>>>28: " + (-1 >>> 28));
        System.out.println("-1>>28: " + (-1 >> 28));
    }
}
