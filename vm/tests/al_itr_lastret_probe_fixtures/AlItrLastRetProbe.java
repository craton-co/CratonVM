import java.util.ArrayList;
import java.util.Iterator;

/**
 * Regression probe for the `ArrayList$Itr` lastRet/cursor field-slot
 * collision bug: `al_itr_last_ret_slot`'s fallback (used when the real
 * `java/util/ArrayList$Itr` class isn't available, i.e. CratonVM's own
 * synthetic layout) defaulted to slot 1 -- the same slot as
 * `AL_ITR_FIELD_CURSOR`. Every `next()` call's `lastRet = cursor` write
 * (using the PRE-increment cursor value) immediately clobbered the
 * `cursor = cursor + 1` write one line above it, so cursor never actually
 * advanced past 0 and `hasNext()` (`cursor < size`) stayed true forever --
 * `next()` kept returning the same first element indefinitely.
 *
 * A Java-level loop draining any ArrayList iterator this way never
 * terminates; found via a real WildFly module (`Module.loadService`
 * draining a ServiceLoader-backed one-element ArrayList) whose real
 * `java.desktop` clinit chain hung at ~100% CPU with no bound.
 */
public class AlItrLastRetProbe {
    public static void main(String[] args) {
        ArrayList<String> one = new ArrayList<>();
        one.add("solo");
        int seen = 0;
        Iterator<String> it1 = one.iterator();
        while (it1.hasNext() && seen < 50) {
            it1.next();
            seen++;
        }
        System.out.println("single_element_seen=" + seen);

        // Iterator.remove() must still see the correct lastRet after the
        // slot moved -- exercises al_itr_last_ret_slot's *other* caller.
        ArrayList<String> four = new ArrayList<>();
        four.add("a"); four.add("b"); four.add("c"); four.add("d");
        Iterator<String> it2 = four.iterator();
        StringBuilder visited = new StringBuilder();
        while (it2.hasNext()) {
            String s = it2.next();
            visited.append(s);
            if (s.equals("b") || s.equals("d")) {
                it2.remove();
            }
        }
        System.out.println("visited=" + visited);
        System.out.println("remaining=" + four);
        System.out.println("DONE");
    }
}
