/**
 * `ThreadGroup.setMaxPriority` and the null-parent constructor, as paired
 * properties diffable against the host JDK.
 *
 * Two behaviours the L4 ThreadGroup work filed and did not fix:
 *
 *  1. CratonVM's `setMaxPriority` clamps the argument into
 *     `[MIN_PRIORITY, MAX_PRIORITY]` and stores it. The JDK does something
 *     different in both halves — this probe pins WHICH, rather than assuming,
 *     because "clamp then take min with the parent" and "return early when out
 *     of range, else take min with the parent" disagree on exactly the inputs
 *     that matter.
 *  2. `new ThreadGroup(null, name)` returns normally under CratonVM and throws
 *     under the JDK.
 *
 * Every line is a paired property. No timing, no identity, no addresses.
 */
public final class ThreadGroupPriorityProbe {

    private static void out(String key, Object value) {
        System.out.println("TGPRI " + key + "=" + value);
    }

    /** Report a throwing expression by exception CLASS, or its value. */
    private static void attempt(String key, Runnable r) {
        try {
            r.run();
            out(key, "no-throw");
        } catch (Throwable t) {
            out(key, t.getClass().getName());
        }
    }

    public static void main(String[] args) {
        ThreadGroup main = Thread.currentThread().getThreadGroup();
        out("main.name", main.getName());
        out("main.max", main.getMaxPriority());

        // ---- 1. The two-step clamp, one step at a time.
        ThreadGroup a = new ThreadGroup("a");
        out("a.parent", a.getParent().getName());
        out("a.max.initial", a.getMaxPriority());

        // In range and below the parent's ceiling: must take.
        a.setMaxPriority(4);
        out("a.max.after4", a.getMaxPriority());

        // Above MAX_PRIORITY. Clamp-then-min would give min(10, parent) — the
        // parent here is `main`, so 10. Return-early would leave 4.
        a.setMaxPriority(Thread.MAX_PRIORITY + 5);
        out("a.max.afterTooHigh", a.getMaxPriority());

        // Below MIN_PRIORITY. Clamp-then-min would give 1. Return-early leaves 4.
        a.setMaxPriority(Thread.MIN_PRIORITY - 5);
        out("a.max.afterTooLow", a.getMaxPriority());

        // In range but ABOVE the current value, still within the parent's
        // ceiling. This separates "may not be raised at all" from "may not
        // exceed the parent".
        a.setMaxPriority(7);
        out("a.max.afterRaiseTo7", a.getMaxPriority());

        // Exactly the bounds.
        ThreadGroup b = new ThreadGroup("b");
        b.setMaxPriority(Thread.MAX_PRIORITY);
        out("b.max.afterMax", b.getMaxPriority());
        b.setMaxPriority(Thread.MIN_PRIORITY);
        out("b.max.afterMin", b.getMaxPriority());

        // ---- 2. The parent ceiling, on a child of a LOWERED group.
        ThreadGroup lo = new ThreadGroup("lo");
        lo.setMaxPriority(3);
        out("lo.max", lo.getMaxPriority());
        ThreadGroup child = new ThreadGroup(lo, "child");
        out("child.max.initial", child.getMaxPriority());
        child.setMaxPriority(9);
        out("child.max.afterRaiseTo9", child.getMaxPriority());
        child.setMaxPriority(2);
        out("child.max.afterLowerTo2", child.getMaxPriority());

        // Lowering a parent AFTER the child exists: does it propagate?
        ThreadGroup p2 = new ThreadGroup("p2");
        ThreadGroup c2 = new ThreadGroup(p2, "c2");
        out("c2.max.before", c2.getMaxPriority());
        p2.setMaxPriority(2);
        out("p2.max.after", p2.getMaxPriority());
        out("c2.max.afterParentLowered", c2.getMaxPriority());

        // Does propagation only LOWER, or does it assign? The JDK's recursion
        // is `for (g : groups) g.setMaxPriority(maxPriority)`, which would RAISE
        // a subgroup that sits below its parent — surprising enough that it has
        // to be measured rather than assumed.
        ThreadGroup p3 = new ThreadGroup("p3");
        ThreadGroup c3 = new ThreadGroup(p3, "c3");
        c3.setMaxPriority(1);
        out("c3.max.lowered", c3.getMaxPriority());
        p3.setMaxPriority(5);
        out("p3.max.after5", p3.getMaxPriority());
        out("c3.max.afterParentRaisedTo5", c3.getMaxPriority());

        // Three deep, so propagation is shown to recurse rather than stop at
        // the direct children.
        ThreadGroup g1 = new ThreadGroup("g1");
        ThreadGroup g2 = new ThreadGroup(g1, "g2");
        ThreadGroup g3 = new ThreadGroup(g2, "g3");
        g1.setMaxPriority(6);
        out("g2.max.afterGrandparent6", g2.getMaxPriority());
        out("g3.max.afterGrandparent6", g3.getMaxPriority());

        // `toString` carries the priority, so it is a second reader of the
        // same state — and one that is easy to leave hard-coded.
        ThreadGroup ts = new ThreadGroup("ts");
        ts.setMaxPriority(6);
        out("ts.toString", ts.toString());

        // ---- 3. What the ceiling is FOR: Thread.setPriority clamps to it.
        ThreadGroup tp = new ThreadGroup("tp");
        tp.setMaxPriority(3);
        Thread t = new Thread(tp, () -> {}, "capped");
        out("t.priority.initial", t.getPriority());
        t.setPriority(Thread.MAX_PRIORITY);
        out("t.priority.afterMax", t.getPriority());
        t.setPriority(2);
        out("t.priority.after2", t.getPriority());

        // ---- 4. The null-parent constructor.
        attempt("err.nullParent", () -> new ThreadGroup(null, "np"));
        // Control: a null NAME is legal on neither side of the comparison for a
        // different reason, so it says whether this is general null-tolerance.
        attempt("err.nullName", () -> new ThreadGroup((String) null));
        attempt("err.nullNameWithParent", () -> new ThreadGroup(main, null));

        System.out.println("TGPRI-COMPLETE");
    }
}
