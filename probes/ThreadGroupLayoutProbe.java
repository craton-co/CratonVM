import java.lang.reflect.Field;

/**
 * A layout probe for {@code java.lang.ThreadGroup}, diffed against the host JDK.
 *
 * CratonVM carries a hand-numbered slot model for `ThreadGroup` and, until
 * 2026-08-05, that model had BOTH field pairs transposed against the image:
 * it said `name=0, parent=1, daemon=2, maxPriority=3` where JDK 21+ declares
 * `parent=0, name=1, maxPriority=2, daemon=3`. A `ThreadGroup` reference
 * written over a `String` reference, and an `int` over an `int`, are invisible
 * to a value-tag overlay check, so nothing measured it.
 *
 * Every section prints VALUES, never "ok" — a probe that prints its own verdict
 * cannot be diffed. The transposition is specifically what makes `getName()`
 * and `getParent()` swap, and `getMaxPriority()` and `isDaemon()` swap, so the
 * assertions below are deliberately PAIRED: a swap that changed only one of a
 * pair would still show up.
 *
 * Run under HotSpot first; its output is the expected file.
 */
public class ThreadGroupLayoutProbe {
    static int sections = 0, failed = 0;

    static void section(String name, Runnable body) {
        sections++;
        try {
            body.run();
        } catch (Throwable t) {
            failed++;
            System.out.println("SECTION-FAILED " + name + ": " + t);
        }
    }

    public static void main(String[] args) throws Exception {
        section("current", ThreadGroupLayoutProbe::current);
        section("construct", ThreadGroupLayoutProbe::construct);
        section("priority", ThreadGroupLayoutProbe::priority);
        section("daemon", ThreadGroupLayoutProbe::daemon);
        section("reflection", ThreadGroupLayoutProbe::reflection);
        section("membership", ThreadGroupLayoutProbe::membership);
        section("errors", ThreadGroupLayoutProbe::errors);
        System.out.println("TGLAYOUT sections=" + sections + " failed=" + failed);
    }

    /** The group the VM handed us, and the whole parent chain up to the root. */
    static void current() {
        ThreadGroup g = Thread.currentThread().getThreadGroup();
        System.out.println("current name=" + g.getName()
                + " maxPri=" + g.getMaxPriority()
                + " daemon=" + g.isDaemon());
        StringBuilder chain = new StringBuilder();
        int depth = 0;
        for (ThreadGroup c = g; c != null; c = c.getParent()) {
            if (depth++ > 0) chain.append(" <- ");
            chain.append(c.getName());
            // getParent() returning a String-shaped object instead of a group
            // is the exact shape of the transposition; walking it proves the
            // chain is made of ThreadGroups.
            if (depth > 8) { chain.append(" ...RUNAWAY"); break; }
        }
        System.out.println("current chain=" + chain + " depth=" + depth);
    }

    static void construct() {
        ThreadGroup parent = new ThreadGroup("tg-parent");
        ThreadGroup child = new ThreadGroup(parent, "tg-child");
        // PAIRED: name and parent are the transposed pair. Print both for each.
        System.out.println("parent name=" + parent.getName()
                + " parentOfParent=" + (parent.getParent() == null ? "null" : parent.getParent().getName()));
        System.out.println("child name=" + child.getName()
                + " parentOfChild=" + (child.getParent() == null ? "null" : child.getParent().getName()));
        System.out.println("child parentIdentity=" + (child.getParent() == parent));
        System.out.println("child nameIsString=" + (((Object) child.getName()) instanceof String));
        // A group's own name must not be readable as its parent, which is what
        // the transposed model produced.
        System.out.println("child nameNotEqualParentName="
                + !child.getName().equals(String.valueOf(child.getParent())));
    }

    static void priority() {
        ThreadGroup parent = new ThreadGroup("pri-parent");
        System.out.println("pri parent max=" + parent.getMaxPriority());
        parent.setMaxPriority(4);
        ThreadGroup child = new ThreadGroup(parent, "pri-child");
        // PAIRED with daemon: maxPriority is an int and daemon a boolean, and
        // the model had them swapped, so an int of 10 landed on the boolean.
        System.out.println("pri parent after=" + parent.getMaxPriority()
                + " parentDaemon=" + parent.isDaemon());
        System.out.println("pri child inherited=" + child.getMaxPriority()
                + " childDaemon=" + child.isDaemon());
        parent.setMaxPriority(Thread.MAX_PRIORITY + 5);
        System.out.println("pri clampHigh=" + parent.getMaxPriority());
        parent.setMaxPriority(Thread.MIN_PRIORITY - 5);
        System.out.println("pri clampLow=" + parent.getMaxPriority());
    }

    static void daemon() {
        ThreadGroup g = new ThreadGroup("dae");
        System.out.println("dae initial=" + g.isDaemon() + " max=" + g.getMaxPriority());
        g.setDaemon(true);
        System.out.println("dae afterTrue=" + g.isDaemon() + " max=" + g.getMaxPriority());
        g.setDaemon(false);
        System.out.println("dae afterFalse=" + g.isDaemon() + " max=" + g.getMaxPriority());
    }

    /**
     * The natives and reflection must agree about the same object. A slot model
     * that disagrees with the image makes the accessor and the reflective read
     * return different things — which is the half a behavioural test alone
     * would miss.
     */
    static void reflection() {
        ThreadGroup parent = new ThreadGroup("ref-parent");
        ThreadGroup g = new ThreadGroup(parent, "ref-child");
        g.setMaxPriority(6);
        for (String fn : new String[] {"parent", "name", "maxPriority", "daemon"}) {
            try {
                Field f = ThreadGroup.class.getDeclaredField(fn);
                f.setAccessible(true);
                Object v = f.get(g);
                String shown = (v instanceof ThreadGroup) ? ("TG:" + ((ThreadGroup) v).getName())
                        : String.valueOf(v);
                System.out.println("ref " + fn + "=" + shown + " type=" + f.getType().getName());
            } catch (NoSuchFieldException e) {
                System.out.println("ref " + fn + "=NO_SUCH_FIELD");
            } catch (Throwable t) {
                System.out.println("ref " + fn + "=THREW " + t.getClass().getName());
            }
        }
        System.out.println("ref accessorsAgree="
                + (g.getName().equals("ref-child")
                   && g.getParent() == parent
                   && g.getMaxPriority() == 6
                   && !g.isDaemon()));
    }

    static void membership() throws RuntimeException {
        ThreadGroup g = new ThreadGroup("mem");
        final String[] seen = new String[1];
        Thread t = new Thread(g, () -> seen[0] = Thread.currentThread().getThreadGroup().getName(),
                "mem-thread");
        t.start();
        try {
            t.join(5000);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
        System.out.println("mem threadSawGroup=" + seen[0]);
        Thread[] arr = new Thread[8];
        int n = g.enumerate(arr);
        System.out.println("mem enumerateAfterJoin=" + n);
        System.out.println("mem activeCountIsNonNegative=" + (g.activeCount() >= 0));
    }

    /** The JDK's error behaviour is part of the contract. */
    static void errors() {
        try {
            new ThreadGroup(null);
            System.out.println("err nullName=NO_THROW");
        } catch (NullPointerException e) {
            System.out.println("err nullName=NPE");
        } catch (Throwable t) {
            System.out.println("err nullName=" + t.getClass().getName());
        }
        try {
            new ThreadGroup(null, "x");
            System.out.println("err nullParent=NO_THROW");
        } catch (NullPointerException e) {
            System.out.println("err nullParent=NPE");
        } catch (Throwable t) {
            System.out.println("err nullParent=" + t.getClass().getName());
        }
    }
}
