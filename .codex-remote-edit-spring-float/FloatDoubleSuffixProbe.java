public class FloatDoubleSuffixProbe {
  static void check(boolean ok, String msg) { if (!ok) throw new AssertionError(msg); }
  public static void main(String[] args) {
    check(Double.parseDouble("1d") == 1.0d, "1d");
    check(Double.parseDouble("3.0d") == 3.0d, "3.0d");
    check(Double.parseDouble("10F") == 10.0d, "10F");
    check(Double.parseDouble("6.0221415E+23d") == 6.0221415E23d, "expo d");
    check(Float.parseFloat("1.25f") == 1.25f, "1.25f");
    check(Float.parseFloat("3.0F") == 3.0f, "3.0F");
    System.out.println("OK suffix parse");
  }
}