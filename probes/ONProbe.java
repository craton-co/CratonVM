import javax.management.ObjectName;

/**
 * H6-1 §7.1's oracle probe, transcribed verbatim from the record so the
 * differential it specifies can be re-run. §7.1: "Run this against CratonVM too
 * — it is the differential that decides §7.3."
 */
public class ONProbe {
    public static void main(String[] a) throws Exception {
        ObjectName n = new ObjectName("java.lang:type=GarbageCollector,name=G1 Young Generation");
        System.out.println("A getCanonicalName         = " + n.getCanonicalName());
        System.out.println("A getKeyPropertyListString = " + n.getKeyPropertyListString());
        System.out.println("A toString                 = " + n.toString());
        System.out.println("A getCanonicalKeyPropList  = " + n.getCanonicalKeyPropertyListString());
        ObjectName m = new ObjectName("d:b=2,a=1,c=3");
        System.out.println("B getCanonicalName         = " + m.getCanonicalName());
        System.out.println("B getKeyPropertyListString = " + m.getKeyPropertyListString());
        System.out.println("B toString                 = " + m.toString());
    }
}
