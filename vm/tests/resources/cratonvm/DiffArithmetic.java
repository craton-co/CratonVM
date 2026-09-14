package cratonvm;

/**
 * Differential testing helper for basic arithmetic.
 * Each static method prints its result to stdout so that the differential
 * harness can compare CratonVM vs HotSpot output line-by-line.
 */
public class DiffArithmetic {
    public static void main(String[] args) {
        System.out.println(add());
        System.out.println(mul());
        System.out.println(div());
        System.out.println(mod_op());
        System.out.println(neg());
        System.out.println(mixed());
    }

    public static int add() { return 10 + 20; }
    public static int mul() { return 6 * 7; }
    public static int div() { return 100 / 4; }
    public static int mod_op() { return 17 % 5; }
    public static int neg() { return -(42); }
    public static int mixed() { return (3 + 4) * (10 - 2) / 2; }
}
