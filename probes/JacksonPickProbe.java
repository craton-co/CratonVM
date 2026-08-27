import java.lang.reflect.Method;

/**
 * Which Jackson does Spring pick?
 *
 * PersonOrderProbe showed both VMs emit identical JSON from Jackson 2 and from
 * Jackson 3 -- so the failing assertion (CratonVM producing Jackson 2's
 * property order where HotSpot produces Jackson 3's) is a converter-SELECTION
 * difference, not a reflection-ordering one.
 *
 * Spring 7 decides between the two with ClassUtils.isPresent() probes against
 * the tools.jackson (v3) and com.fasterxml (v2) mapper classes. This prints
 * every input to that decision and the decision itself.
 */
public class JacksonPickProbe {

    static void present(String cls) {
        ClassLoader cl = JacksonPickProbe.class.getClassLoader();
        String viaForName;
        try {
            Class.forName(cls, false, cl);
            viaForName = "yes";
        } catch (Throwable t) {
            viaForName = "NO (" + t.getClass().getSimpleName() + ")";
        }
        String viaSpring;
        try {
            Class<?> cu = Class.forName("org.springframework.util.ClassUtils");
            Method m = cu.getMethod("isPresent", String.class, ClassLoader.class);
            viaSpring = String.valueOf(m.invoke(null, cls, cl));
        } catch (Throwable t) {
            viaSpring = "ERR " + t;
        }
        System.out.printf("  %-52s forName=%-22s ClassUtils.isPresent=%s%n",
                cls, viaForName, viaSpring);
    }

    public static void main(String[] args) throws Exception {
        System.out.println("== class presence ==");
        for (String c : new String[] {
                "tools.jackson.databind.ObjectMapper",
                "tools.jackson.databind.json.JsonMapper",
                "com.fasterxml.jackson.databind.ObjectMapper",
                "tools.jackson.dataformat.xml.XmlMapper",
                "com.fasterxml.jackson.dataformat.xml.XmlMapper",
        }) {
            present(c);
        }

        // RestTemplate's default converter list is assembled with exactly the
        // same isPresent() gates the MVC side uses, so it reads the decision
        // out without standing up a servlet context.
        System.out.println("== RestTemplate default converters ==");
        try {
            Class<?> rt = Class.forName("org.springframework.web.client.RestTemplate");
            Object t = rt.getDeclaredConstructor().newInstance();
            Method g = rt.getMethod("getMessageConverters");
            for (Object c : (java.util.List<?>) g.invoke(t)) {
                System.out.println("  " + c.getClass().getName());
            }
        } catch (Throwable t) {
            System.out.println("  UNAVAILABLE " + t);
        }

        // The ServiceLoader half: Jackson 3 discovers modules this way, and a
        // VM whose ServiceLoader returns nothing would silently behave like a
        // bare mapper even when the classes are present.
        System.out.println("== ServiceLoader: jackson3 modules ==");
        try {
            Class<?> modCls = Class.forName("tools.jackson.databind.JacksonModule");
            int n = 0;
            for (Object o : java.util.ServiceLoader.load(modCls)) {
                System.out.println("  " + o.getClass().getName());
                n++;
            }
            System.out.println("  count=" + n);
        } catch (Throwable t) {
            System.out.println("  UNAVAILABLE " + t);
        }
    }
}
