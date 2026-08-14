public class NsmeProbe {
  public static void main(String[] a) {
    try { new Lib().widen(true); } catch (Throwable t) { show(t); }
    try { Lib.calc(1, null, null); } catch (Throwable t) { show(t); }
    try { new Lib().plain(); } catch (Throwable t) { show(t); }
  }
  static void show(Throwable t) {
    System.out.println("NSME_CLASS=" + t.getClass().getName());
    System.out.println("NSME_MESSAGE=[" + t.getMessage() + "]");
  }
}
