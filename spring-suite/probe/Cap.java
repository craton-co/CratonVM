import java.util.ArrayList;
public class Cap {
  public static void main(String[] a) {
    try { new ArrayList<>(-1); System.out.println("neg: NO THROW (BUG)"); }
    catch (Throwable t){ System.out.println("neg: "+t.getClass().getSimpleName()+": "+t.getMessage()); }
    ArrayList<Integer> l = new ArrayList<>(1000);
    for (int i=0;i<5000;i++) l.add(i);
    System.out.println("normal(1000)+5000adds: size="+l.size()+" first="+l.get(0)+" last="+l.get(4999));
    ArrayList<Integer> z = new ArrayList<>(0);
    z.add(7); System.out.println("zero-cap: size="+z.size()+" v="+z.get(0));
    System.out.println("DONE-CAP");
  }
}
