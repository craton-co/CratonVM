package cratonvm;
import java.math.*;
public class TckBigMath {
    public static int bi_add() { return BigInteger.valueOf(2).add(BigInteger.valueOf(3)).intValue() == 5 ? 1 : 0; }
    public static int bi_subtract() { return BigInteger.valueOf(10).subtract(BigInteger.valueOf(3)).intValue() == 7 ? 1 : 0; }
    public static int bi_multiply() { return BigInteger.valueOf(6).multiply(BigInteger.valueOf(7)).intValue() == 42 ? 1 : 0; }
    public static int bi_divide() { return BigInteger.valueOf(10).divide(BigInteger.valueOf(2)).intValue() == 5 ? 1 : 0; }
    public static int bi_mod() { return BigInteger.valueOf(10).mod(BigInteger.valueOf(3)).intValue() == 1 ? 1 : 0; }
    public static int bi_compareTo() { return BigInteger.valueOf(5).compareTo(BigInteger.valueOf(10)) < 0 ? 1 : 0; }
    public static int bi_toString() { return "42".equals(BigInteger.valueOf(42).toString()) ? 1 : 0; }
    public static int bi_valueOf() { return BigInteger.valueOf(100).intValue() == 100 ? 1 : 0; }
    public static int bi_bitLength() { return BigInteger.valueOf(255).bitLength() == 8 ? 1 : 0; }
    public static int bd_add() { return BigDecimal.valueOf(1.5).add(BigDecimal.valueOf(2.5)).compareTo(BigDecimal.valueOf(4.0)) == 0 ? 1 : 0; }
    public static int bd_subtract() { return BigDecimal.valueOf(10).subtract(BigDecimal.valueOf(3)).compareTo(BigDecimal.valueOf(7)) == 0 ? 1 : 0; }
    public static int bd_multiply() { return BigDecimal.valueOf(3).multiply(BigDecimal.valueOf(4)).compareTo(BigDecimal.valueOf(12)) == 0 ? 1 : 0; }
    public static int bd_scale() { return new BigDecimal("1.23").scale() == 2 ? 1 : 0; }
    public static int bd_toString() { return "42".equals(BigDecimal.valueOf(42).toString()) ? 1 : 0; }
    public static int bd_compareTo() { return BigDecimal.valueOf(5).compareTo(BigDecimal.valueOf(10)) < 0 ? 1 : 0; }
}
