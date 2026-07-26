public class TxCtl {
  public static void main(String[] a) {
    try { Class.forName("com.arjuna.ats.arjuna.coordinator.TxControl");
      System.out.println("TxControl loaded OK"); }
    catch (Throwable t) {
      System.out.println("LOAD FAILED: " + t);
      Throwable c = t.getCause();
      while (c != null) { System.out.println("CAUSE: " + c); 
        for (StackTraceElement e : c.getStackTrace()) System.out.println("   at " + e);
        c = c.getCause(); }
    }
  }
}
