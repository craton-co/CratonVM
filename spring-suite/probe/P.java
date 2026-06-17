public class P {
  public static void main(String[] a) throws Exception {
    String which = a[0];
    try {
      switch (which) {
        case "int":    { int[] x = new int[Integer.MAX_VALUE]; System.out.println("OK len="+x.length); break; }
        case "obj":    { Object[] x = new Object[Integer.MAX_VALUE]; System.out.println("OK len="+x.length); break; }
        case "list":   { Object o = new java.util.ArrayList<>(Integer.MAX_VALUE); System.out.println("OK "+o); break; }
        case "intbig": { int[] x = new int[2000000000]; System.out.println("OK len="+x.length); break; }
      }
    } catch (Throwable t) { System.out.println("CAUGHT "+t.getClass().getName()+": "+t.getMessage()); }
    System.out.println("END");
  }
}
