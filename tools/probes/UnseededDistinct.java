import java.util.HashSet;
import java.util.Random;
import java.util.Set;

/**
 * `new Random()` no longer draws OS entropy — it uses the JDK's specified
 * `seedUniquifier() ^ System.nanoTime()`. `RandomSpec`'s `unseeded-distinct`
 * row only compares TWO instances, which a broken uniquifier could still pass
 * whenever the clock happened to tick between them.
 *
 * <p>This is the stronger claim: N successive unseeded `Random`s, constructed as
 * fast as the loop can go, must all produce different first values. The
 * uniquifier's multiply is what guarantees that when `System.nanoTime()` has
 * NOT advanced, so a run tight enough to hit the same nanosecond repeatedly is
 * exactly the case worth testing.
 */
public class UnseededDistinct {
    public static void main(String[] args) {
        int n = Integer.getInteger("n", 200_000);

        Set<Long> firsts = new HashSet<>(n * 2);
        for (int i = 0; i < n; i++) {
            firsts.add(new Random().nextLong());
        }
        System.out.println("CK unseeded n=" + n + " distinctFirstValues=" + firsts.size()
                + " allDistinct=" + (firsts.size() == n));

        // Same again but forcing the tightest possible construction loop, with
        // no work between constructions at all.
        Set<Long> seeds = new HashSet<>(n * 2);
        Random[] rs = new Random[1000];
        for (int i = 0; i < rs.length; i++) {
            rs[i] = new Random();
        }
        for (Random r : rs) {
            seeds.add(r.nextLong());
        }
        System.out.println("CK burst n=" + rs.length + " distinct=" + seeds.size()
                + " allDistinct=" + (seeds.size() == rs.length));
    }
}
