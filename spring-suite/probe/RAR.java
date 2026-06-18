public class RAR {
  public static void main(String[] a){
    try {
      Object r = Class.forName("org.springframework.core.ReactiveAdapterRegistry").getDeclaredConstructor().newInstance();
      System.out.println("RAR OK: "+r.getClass().getName());
    } catch(Throwable t){
      Throwable c=t; while(c.getCause()!=null) c=c.getCause();
      System.out.println("RAR FAILED: "+t.getClass().getSimpleName()+" -> root "+c.getClass().getName()+": "+c.getMessage());
    }
  }
}
