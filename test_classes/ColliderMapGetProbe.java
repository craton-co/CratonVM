import java.util.HashMap;
import java.util.Map;
import java.util.Objects;
import java.util.stream.Collectors;
import java.util.stream.Stream;

/** Narrows the "3arg collider merge" failure: is it toMap, or plain Map.get
 *  with hash-colliding keys? */
public class ColliderMapGetProbe {
    record Collider(String name) {
        @Override public int hashCode() { return 42; }
        @Override public boolean equals(Object o) {
            return o instanceof Collider c && Objects.equals(name, c.name);
        }
    }
    public static void main(String[] args) {
        Map<Collider, Integer> plain = new HashMap<>();
        plain.put(new Collider("p"), 1);
        plain.put(new Collider("q"), 9);
        System.out.println("PLAIN size=" + plain.size()
                + " get(p)=" + plain.get(new Collider("p"))
                + " get(q)=" + plain.get(new Collider("q")));

        Map<Collider, Integer> viaToMap =
                Stream.of(new Collider("p"), new Collider("q"), new Collider("p"))
                        .collect(Collectors.toMap(c -> c, c -> 1, Integer::sum));
        System.out.println("TOMAP size=" + viaToMap.size()
                + " get(p)=" + viaToMap.get(new Collider("p"))
                + " get(q)=" + viaToMap.get(new Collider("q"))
                + " entries=" + viaToMap);

        Map<Collider, Integer> noCollide = new HashMap<>();
        noCollide.put(new Collider("p"), 2);
        System.out.println("SINGLE get(p)=" + noCollide.get(new Collider("p")));
    }
}
