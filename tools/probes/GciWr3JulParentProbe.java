// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.Reference;
import java.util.logging.LogManager;
import java.util.logging.Logger;

/**
 * gci i3/wr: single-threaded parent linking of {@code java.util.logging}
 * loggers, to separate the {@code --compatible} DIFF of vi2-wr C
 * ({@code wr2_jrace_par_off_cpt_*}: every racing leaf reported the ROOT as
 * its parent where HotSpot reports the dotted parent) from the concurrency
 * that row was written for. Page
 * {@code docs/known-issues/gc/gci-i3wr-compatible-jul-leaf-is-parented-to-the-root-under-a-parents-first-manager-20261012.md}.
 *
 * Mode {@code default} uses the JDK's {@code LogManager}; mode {@code parents}
 * selects the same parents-first manager as {@code GciWr2JulRaceProbe} (its
 * {@code addLogger} creates the dotted parent with {@code Logger.getLogger}
 * before {@code super.addLogger}). Every case runs on the main thread, so a
 * DIFF here is not a race.
 *
 * Cases (one stdout line each, deterministic):
 *  c1 parent created first, then the child: child's parent is the parent;
 *  c2 child created first, then the parent: the parent insertion re-links the
 *     child ({@code LogNode.walkAndSetParent});
 *  c3 a leaf whose parent nobody created: {@code default} answers the root
 *     ({@code ""}), {@code parents} answers the dotted parent the manager made;
 *  c4 the manager's view: {@code LogManager.getLogger} of c3's parent name is
 *     null ({@code default}) or that same parent object ({@code parents});
 *  c5 an anonymous logger's parent is the root;
 *  c6 the root's parent is null.
 */
public class GciWr3JulParentProbe {
    /** Same shape as {@code GciWr2JulRaceProbe.ParentsFirstManager}. */
    public static class ParentsFirstManager extends LogManager {
        public ParentsFirstManager() {
            super();
        }

        @Override
        public synchronized boolean addLogger(Logger logger) {
            String name = logger.getName();
            int dot = name == null ? -1 : name.lastIndexOf('.');
            Logger parent = null;
            if (dot > 0) {
                parent = Logger.getLogger(name.substring(0, dot));
            }
            boolean added = super.addLogger(logger);
            Reference.reachabilityFence(parent);
            return added;
        }
    }

    private static String nameOf(Logger l) {
        if (l == null) {
            return "null";
        }
        String n = l.getName();
        return n == null ? "<null-name>" : "'" + n + "'";
    }

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "default";
        if (mode.equals("parents")) {
            System.setProperty("java.util.logging.manager", ParentsFirstManager.class.getName());
        } else if (!mode.equals("default")) {
            System.out.println("usage: GciWr3JulParentProbe default|parents");
            System.exit(2);
        }

        Logger p1 = Logger.getLogger("gci.wr3.c1");
        Logger c1 = Logger.getLogger("gci.wr3.c1.kid");
        System.out.println("c1 parent=" + nameOf(c1.getParent()) + " same=" + (c1.getParent() == p1));

        Logger c2 = Logger.getLogger("gci.wr3.c2.kid");
        String before = nameOf(c2.getParent());
        Logger p2 = Logger.getLogger("gci.wr3.c2");
        System.out.println("c2 before=" + before + " after=" + nameOf(c2.getParent())
                + " same=" + (c2.getParent() == p2));

        Logger c3 = Logger.getLogger("gci.wr3.c3.leaf");
        System.out.println("c3 parent=" + nameOf(c3.getParent()));

        Logger viaManager = LogManager.getLogManager().getLogger("gci.wr3.c3");
        System.out.println("c4 manager=" + nameOf(viaManager)
                + " same=" + (viaManager != null && viaManager == c3.getParent()));

        Logger anon = Logger.getAnonymousLogger();
        System.out.println("c5 anon parent=" + nameOf(anon.getParent()));

        Logger root = LogManager.getLogManager().getLogger("");
        System.out.println("c6 root=" + nameOf(root) + " parent=" + nameOf(root == null ? null : root.getParent()));

        System.out.println("jul-parent: mode=" + mode + " manager="
                + LogManager.getLogManager().getClass().getSimpleName());
        Reference.reachabilityFence(p1);
        Reference.reachabilityFence(p2);
        Reference.reachabilityFence(c1);
        Reference.reachabilityFence(c2);
        Reference.reachabilityFence(c3);
    }
}
