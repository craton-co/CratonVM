import kotlin.Metadata;

/** Dump the raw @kotlin.Metadata annotation values so we can compare the proto
 *  payload CratonVM exposes to kotlin-reflect against HotSpot's. */
public class MetaProbe {
    public static void main(String[] args) throws Exception {
        Class<?> c = Class.forName("org.springframework.core.MethodParameterKotlinTests");
        Metadata md = c.getAnnotation(Metadata.class);
        if (md == null) { System.out.println("NO @Metadata"); return; }
        System.out.println("k=" + md.k());
        System.out.println("mv=" + java.util.Arrays.toString(md.mv()));
        System.out.println("xi=" + md.xi() + " xs='" + md.xs() + "' pn='" + md.pn() + "'");
        String[] d1 = md.d1();
        System.out.println("d1.length=" + d1.length);
        for (int i = 0; i < d1.length; i++) {
            System.out.println("  d1[" + i + "] len=" + d1[i].length() + " hash=" + d1[i].hashCode());
        }
        String[] d2 = md.d2();
        System.out.println("d2.length=" + d2.length);
        StringBuilder sb = new StringBuilder();
        for (String s : d2) sb.append('|').append(s);
        System.out.println("d2.joined.hash=" + sb.toString().hashCode() + " joinedLen=" + sb.length());
        System.out.println("d2=" + java.util.Arrays.toString(d2));
    }
}
