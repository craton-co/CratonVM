import java.util.*;
public class P5Exc {
  static class MyEx extends RuntimeException { MyEx(String m){super(m);} }
  public static void main(String[] a) {
    try { throw new MyEx("boom"); } catch (MyEx e) { System.out.println(e + " " + e.getStackTrace()[0].getMethodName()); }
    try { Object o = null; o.hashCode(); } catch (NullPointerException e) { System.out.println("npe " + e.getMessage()); }
    try { int[] x = new int[2]; x[3]=1; } catch (ArrayIndexOutOfBoundsException e) { System.out.println(e.getMessage()); }
    try { Object s = "x"; Integer i = (Integer) s; } catch (ClassCastException e) { System.out.println(e.getMessage()); }
    try { Integer.parseInt("zz"); } catch (NumberFormatException e) { System.out.println(e.getMessage()); }
    try { new ArrayList<Integer>().iterator().next(); } catch (NoSuchElementException e) { System.out.println("nse " + e); }
    try { List.of(1).add(2); } catch (UnsupportedOperationException e) { System.out.println("uoe " + e); }
    try { throw new IllegalStateException("ise", new java.io.IOException("io")); } catch (IllegalStateException e) { System.out.println(e + " cause " + e.getCause()); }
    try { Class.forName("no.such.Klass"); } catch (ClassNotFoundException e) { System.out.println("cnf " + e.getMessage()); }
    try { int z = 0; System.out.println(1 / z); } catch (ArithmeticException e) { System.out.println(e); }
    Throwable t = new Exception("st"); System.out.println(t.getStackTrace().length > 0);
  }
}
