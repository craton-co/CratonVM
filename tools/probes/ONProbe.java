import javax.management.ObjectName;

/**
 * H0-1 section 3: `ObjectName` no longer indexes a real layout by number.
 * Also H6-1 section 7.1's oracle probe. Every line is deterministic.
 */
public class ONProbe {
    public static void main(String[] a) throws Exception {
        ObjectName n = new ObjectName("java.lang:type=GarbageCollector,name=G1 Young Generation");
        System.out.println("A getCanonicalName         = " + n.getCanonicalName());
        System.out.println("A getKeyPropertyListString = " + n.getKeyPropertyListString());
        System.out.println("A toString                 = " + n.toString());
        System.out.println("A getCanonicalKeyPropList  = " + n.getCanonicalKeyPropertyListString());
        System.out.println("A getDomain                = " + n.getDomain());
        System.out.println("A getKeyProperty(type)     = " + n.getKeyProperty("type"));
        ObjectName m = new ObjectName("d:b=2,a=1,c=3");
        System.out.println("B getCanonicalName         = " + m.getCanonicalName());
        System.out.println("B getKeyPropertyListString = " + m.getKeyPropertyListString());
        System.out.println("B toString                 = " + m.toString());
        System.out.println("RESULT done");
    }
}
