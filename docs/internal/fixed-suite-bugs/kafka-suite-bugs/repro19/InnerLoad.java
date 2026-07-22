public class InnerLoad {
  public static void main(String[] a) throws Exception {
    String[] names = {"java.lang.Thread$FieldHolder","java.util.HashMap$Node","java.lang.Thread$State"};
    for (String n : names) {
      try { Class<?> c = Class.forName(n, false, null); System.out.println("OK   "+n+" -> "+c); }
      catch (Throwable t) { System.out.println("FAIL "+n+" -> "+t.getClass().getSimpleName()+": "+t.getMessage()); }
    }
  }
}
