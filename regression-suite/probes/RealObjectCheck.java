import java.util.*;
import java.util.concurrent.*;

/**
 * Which collection surfaces hand back REAL JDK objects under `--jdk-only`?
 *
 * G85-1 §3a: a registrar is safe to retag only if the OBJECTS its methods
 * receive are real too. Rule 4 ("concrete bytecode + incomplete replacement ->
 * delete from the strict path") assumes the real bytecode can READ its
 * receiver; where the VM mints the object itself with its own field layout,
 * dropping the shim leaves real bytecode reading fields that were never
 * populated. That is how retagging `LinkedHashMap` produced
 * `new ArrayList<>(lh.keySet())` -> `toArray()` == null.
 *
 * The registry dump cannot answer this: it describes REGISTRATIONS, not
 * instances. This probe answers it directly — construct the thing, ask what it
 * IS, and ask a derived view the same question. Diff against HotSpot: a class
 * name that matches is a real object; one that differs (or a
 * `cratonvm/internal/*` name) is a stand-in, and its registrar must not be
 * retagged until the object is real too.
 *
 * Run under both VMs and diff. Every line is a class name, so the comparison
 * is exact and machine-independent.
 *
 * ==========================================================================
 * READ THIS BEFORE TRUSTING A GREEN RESULT. This probe answers a NARROWER
 * question than "is it safe to retag", and the difference is the whole trap.
 *
 * Measured 2026-08-19: this probe reports 0 of 31 diverging — every object and
 * every derived view above already carries its REAL JDK class name under
 * `--jdk-only`, `LinkedHashMap.keySet()` included. And yet retagging
 * `register_linked_hashmap_natives` broke `new ArrayList<>(lh.keySet())` with
 * `toArray() == null` (G85-1 §3a).
 *
 * Both facts are true, because CLASS IDENTITY IS NOT STATE OWNERSHIP. The
 * `LinkedHashMap` really is a `java.util.LinkedHashMap`; what our `put()` shim
 * maintains is a side structure, not the real `table`/`head`/`tail` fields. So
 * real `keySet()` bytecode runs against a real object whose real fields were
 * never populated, and finds an empty map.
 *
 * A green line here therefore means "the class is real" and NOT "the real
 * bytecode can read it". The only test that answers the second question is
 * running the real bytecode with the shim dropped — which is the arms.
 *
 * Kept anyway, because a RED line is conclusive in the other direction: a
 * stand-in class name (or a `cratonvm/internal/*` one) is proof the registrar
 * must not be retagged. It is a cheap disqualifier, not a certificate.
 * ==========================================================================
 */
public class RealObjectCheck {
    static void t(String label, Object o) {
        System.out.println("O " + label + " = " + (o == null ? "<null>" : o.getClass().getName()));
    }

    public static void main(String[] a) {
        // Concrete containers.
        t("ArrayList", new ArrayList<>(List.of("a")));
        t("LinkedList", new LinkedList<>(List.of("a")));
        t("Vector", new Vector<>(List.of("a")));
        t("Stack", new Stack<String>());
        t("ArrayDeque", new ArrayDeque<>(List.of("a")));
        t("PriorityQueue", new PriorityQueue<>(List.of("a")));
        t("HashMap", new HashMap<>(Map.of("k", "v")));
        t("LinkedHashMap", new LinkedHashMap<>(Map.of("k", "v")));
        t("TreeMap", new TreeMap<>(Map.of("k", "v")));
        t("HashSet", new HashSet<>(List.of("a")));
        t("LinkedHashSet", new LinkedHashSet<>(List.of("a")));
        t("TreeSet", new TreeSet<>(List.of("a")));
        t("ConcurrentHashMap", new ConcurrentHashMap<>(Map.of("k", "v")));

        // DERIVED VIEWS — the half that broke. A real container can still hand
        // back a fabricated view, and it is the view the shim was serving.
        t("HashMap.keySet", new HashMap<>(Map.of("k", "v")).keySet());
        t("HashMap.values", new HashMap<>(Map.of("k", "v")).values());
        t("HashMap.entrySet", new HashMap<>(Map.of("k", "v")).entrySet());
        t("LinkedHashMap.keySet", new LinkedHashMap<>(Map.of("k", "v")).keySet());
        t("TreeMap.keySet", new TreeMap<>(Map.of("k", "v")).keySet());
        t("ConcurrentHashMap.keySet", new ConcurrentHashMap<>(Map.of("k", "v")).keySet());
        t("ArrayList.subList", new ArrayList<>(List.of("a", "b", "c")).subList(0, 2));
        t("ArrayList.iterator", new ArrayList<>(List.of("a")).iterator());
        t("HashSet.iterator", new HashSet<>(List.of("a")).iterator());

        // Factories and wrappers, for contrast: these were measured REAL.
        t("List.of", List.of("a"));
        t("Map.of", Map.of("k", "v"));
        t("Set.of", Set.of("a"));
        t("unmodifiableList", Collections.unmodifiableList(new ArrayList<>(List.of("a"))));
        t("unmodifiableMap", Collections.unmodifiableMap(new HashMap<>(Map.of("k", "v"))));
        t("unmodifiableSet", Collections.unmodifiableSet(new HashSet<>(List.of("a"))));
        t("emptyList", Collections.emptyList());
        t("singletonList", Collections.singletonList("a"));

        System.out.println("O done = 1");
    }
}
