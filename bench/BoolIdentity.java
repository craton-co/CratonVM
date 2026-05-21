import java.util.HashMap;
import java.util.Map;

public class BoolIdentity {
    public static void main(String[] args) {
        Boolean vt = Boolean.valueOf(true);
        Boolean vf = Boolean.valueOf(false);
        System.out.println("Boolean.valueOf(true) == Boolean.TRUE: " + (vt == Boolean.TRUE));
        System.out.println("Boolean.valueOf(false) == Boolean.FALSE: " + (vf == Boolean.FALSE));

        boolean prim = true;
        Boolean boxed = prim;  // autobox
        System.out.println("autobox(true) == Boolean.TRUE: " + (boxed == Boolean.TRUE));

        Object o = Boolean.TRUE;
        Map<String,Object> m = new HashMap<>();
        m.put("k", true);   // autobox into map
        System.out.println("map.get(k) == Boolean.TRUE: " + (m.get("k") == Boolean.TRUE));
        System.out.println("map.get(k) == o: " + (m.get("k") == o));

        Boolean parsed = Boolean.valueOf("true");
        System.out.println("Boolean.valueOf(\"true\") == Boolean.TRUE: " + (parsed == Boolean.TRUE));

        // Boolean.parseBoolean then valueOf
        Boolean b2 = Boolean.valueOf(Boolean.parseBoolean("true"));
        System.out.println("valueOf(parseBoolean(true)) == TRUE: " + (b2 == Boolean.TRUE));
    }
}
