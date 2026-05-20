public class ArrayFillRepro {
  public static void main(String[] a) {
    byte[] buf = new byte[65536];
    java.util.Arrays.fill(buf, (byte) -1);
    int n = 0;
    for (byte b : buf) n += b;
    System.out.println("sum=" + n);
  }
}
