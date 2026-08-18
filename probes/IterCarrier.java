import java.util.*;
public class IterCarrier {
    public static void main(String[] a) {
        Map<String,String> m = new HashMap<>();
        m.put("k","v");
        Collection<String> v = m.values();
        System.out.println("values class    = " + v.getClass().getName());
        System.out.println("values.iterator = " + v.iterator().getClass().getName());
        Set<String> ks = m.keySet();
        System.out.println("keySet.iterator = " + ks.iterator().getClass().getName());
        List<String> al = new ArrayList<>(); al.add("x");
        System.out.println("arraylist.iterator = " + al.iterator().getClass().getName());
        Collection<String> asColl = al;
        System.out.println("via Collection door = " + asColl.iterator().getClass().getName());
    }
}
