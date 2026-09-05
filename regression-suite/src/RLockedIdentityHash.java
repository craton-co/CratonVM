// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.HashMap;
import java.util.IdentityHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.CountDownLatch;

/**
 * Regression: an object's identity hash must not change when its monitor is
 * taken or released.
 *
 * The mark word carries the identity hash only in its NEUTRAL state — a
 * THIN_LOCKED payload is an owner plus a recursion count and an INFLATED
 * payload is a monitor pointer. Hashing a locked object therefore has to
 * inflate and displace the hash into the monitor (HotSpot's
 * `ObjectSynchronizer::FastHashCode`). Before the fix this vector pins, the VM
 * instead answered `Integer.MAX_VALUE` for EVERY locked-and-not-yet-hashed
 * object and minted a fresh value at the next unlock.
 *
 * Nothing about that throws. It is only ever observed as a `Map` that cannot
 * find its own entry, which is why it surfaced twice with unrecognisably
 * different faces:
 *
 *   * `org.apache.naming.ContextBindings` parks a webapp's JNDI context in a
 *     `ConcurrentHashMap` keyed by the catalina `Context`, from inside
 *     `LifecycleBase.start()` — which is `synchronized` on that very object.
 *     The later lookup missed and `getContext()` returned null, so Tomcat's
 *     `TestNamingContext` died on a NullPointerException two frames away. See
 *     testnamingcontext-contextbindings-lookup-returns-null-CLOSED.md.
 *   * Spring Boot's `TomcatWebServer` parks connectors in a
 *     `Map<Service,Connector[]>` from the same `synchronized` method. The missed
 *     lookup made `Tomcat.getConnector()` fabricate a default port-8080
 *     connector for an already-running service, and every embedded-Tomcat test
 *     failed with what read like a port conflict and was not one.
 *
 * Only stability booleans and map sizes are printed. The hash VALUES are
 * legitimately VM-specific, so printing one would make the HotSpot output diff
 * fail for a correct VM.
 */
public class RLockedIdentityHash {

    static int checks = 0;

    static void eq(boolean actual, boolean expected, String what) {
        if (actual != expected) {
            throw new AssertionError(what + ": expected " + expected + " got " + actual);
        }
        checks++;
    }

    static void eqInt(int actual, int expected, String what) {
        if (actual != expected) {
            throw new AssertionError(what + ": expected " + expected + " got " + actual);
        }
        checks++;
    }

    public static void main(String[] args) throws Exception {
        hashedBeforeLocking();
        hashedWhileLocked();
        hashedWhileLockedRecursively();
        hashedWhileInflated();
        everyLockedObjectGetsItsOwnHash();
        mapKeyedUnderItsOwnLock();

        System.out.println("CK RLockedIdentityHash checks=" + checks);
        System.out.println("PASS RLockedIdentityHash");
    }

    /** The easy direction: hash first, then lock. Was already correct. */
    static void hashedBeforeLocking() {
        Object o = new Object();
        int before = System.identityHashCode(o);
        int inside;
        synchronized (o) {
            inside = System.identityHashCode(o);
        }
        int after = System.identityHashCode(o);
        eq(before == inside, true, "hash taken before locking survives the lock");
        eq(inside == after, true, "hash taken before locking survives the unlock");
    }

    /** The regressed direction: FIRST hash happens while the monitor is held. */
    static void hashedWhileLocked() {
        Object o = new Object();
        int inside;
        synchronized (o) {
            inside = System.identityHashCode(o);
        }
        int after = System.identityHashCode(o);
        eq(inside == after, true, "first hash taken under the lock survives the unlock");
        eq(inside == o.hashCode(), true, "Object.hashCode agrees with identityHashCode");
    }

    /** Recursive entry: the mark word carries a recursion count, not a hash. */
    static void hashedWhileLockedRecursively() {
        Object o = new Object();
        int deep;
        synchronized (o) {
            synchronized (o) {
                synchronized (o) {
                    deep = System.identityHashCode(o);
                }
            }
        }
        eq(deep == System.identityHashCode(o), true, "hash taken 3 frames deep survives");
    }

    /**
     * Contended entry, so the monitor is INFLATED rather than thin-locked: a
     * different mark-word payload, and a separate arm of the same decode.
     */
    static void hashedWhileInflated() throws Exception {
        final Object o = new Object();
        final CountDownLatch held = new CountDownLatch(1);
        final CountDownLatch release = new CountDownLatch(1);
        Thread owner = new Thread(() -> {
            synchronized (o) {
                held.countDown();
                try {
                    release.await();
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                }
            }
        });
        owner.start();
        held.await();
        // Contend for it from here so the monitor inflates, then hash it while
        // the OTHER thread owns it.
        Thread contender = new Thread(() -> {
            synchronized (o) {
                // no-op; the point is to block and force inflation
            }
        });
        contender.start();
        Thread.sleep(50);
        int whileOwnedByAnother = System.identityHashCode(o);
        release.countDown();
        owner.join(60_000);
        contender.join(60_000);
        eq(owner.isAlive() || contender.isAlive(), false, "helper threads finished");
        eq(whileOwnedByAnother == System.identityHashCode(o), true,
                "hash of an inflated monitor's object survives deflation pressure");
    }

    /**
     * The failure was not just instability: every locked-then-hashed object in
     * the process answered the SAME value. 64 distinct objects must produce
     * (near-)distinct hashes — asserted as "not all equal", since a legitimate
     * hash function may collide.
     */
    static void everyLockedObjectGetsItsOwnHash() {
        List<Object> objs = new ArrayList<>();
        List<Integer> hashes = new ArrayList<>();
        for (int i = 0; i < 64; i++) {
            Object o = new Object();
            objs.add(o);
            synchronized (o) {
                hashes.add(System.identityHashCode(o));
            }
        }
        int distinct = new java.util.HashSet<>(hashes).size();
        eq(distinct > 1, true, "64 locked objects do not all share one hash");
        // And each one still answers its own value after the unlock.
        boolean allStable = true;
        for (int i = 0; i < objs.size(); i++) {
            if (System.identityHashCode(objs.get(i)) != hashes.get(i)) {
                allStable = false;
            }
        }
        eq(allStable, true, "every locked-then-hashed object keeps its hash");
    }

    /**
     * The shape both production witnesses had: the entry is filed while the key
     * is locked and read back after the unlock. Asserted for the three map
     * implementations the two witnesses used between them.
     */
    static void mapKeyedUnderItsOwnLock() {
        Object k1 = new Object();
        Map<Object, String> hm = new HashMap<>();
        synchronized (k1) {
            hm.put(k1, "value");
        }
        eq("value".equals(hm.get(k1)), true, "HashMap finds a key put under its own lock");
        hm.put(k1, "again");
        eqInt(hm.size(), 1, "re-putting the same key does not grow the HashMap");

        Object k2 = new Object();
        Map<Object, String> chm = new ConcurrentHashMap<>();
        synchronized (k2) {
            chm.put(k2, "value");
        }
        eq("value".equals(chm.get(k2)), true, "ConcurrentHashMap finds a key put under its own lock");
        chm.put(k2, "again");
        eqInt(chm.size(), 1, "re-putting the same key does not grow the ConcurrentHashMap");

        Object k3 = new Object();
        Map<Object, String> ihm = new IdentityHashMap<>();
        synchronized (k3) {
            ihm.put(k3, "value");
        }
        eq("value".equals(ihm.get(k3)), true, "IdentityHashMap finds a key put under its own lock");
        eqInt(ihm.size(), 1, "IdentityHashMap has exactly one entry");

        // And the entrySet the map yields must agree with the map's own get —
        // the asymmetry that made this look like a lost entry rather than a
        // wrong hash.
        for (Map.Entry<Object, String> e : hm.entrySet()) {
            eq(hm.get(e.getKey()) != null, true, "get() finds every key entrySet() yields");
        }
    }
}
