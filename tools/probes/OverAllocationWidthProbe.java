import java.io.IOException;
import java.net.InetSocketAddress;
import java.nio.channels.AsynchronousServerSocketChannel;
import java.nio.channels.FileChannel;
import java.nio.channels.FileLock;
import java.nio.channels.SelectionKey;
import java.nio.channels.Selector;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import java.util.ArrayList;
import java.util.Iterator;
import java.util.List;
import java.util.NavigableSet;
import java.util.Set;
import java.util.SortedSet;
import java.util.TreeMap;
import java.util.TreeSet;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ConcurrentSkipListMap;
import java.util.concurrent.TimeUnit;

/**
 * Paired probe for the LIVE over-allocations of W7-66-live-over-allocations.md
 * — a native asking the allocator for MORE slots than the real JDK class
 * declares, on the eleven-triple census W7-59-layout-detector-coverage.md took.
 *
 * <p>Companion to probes/SlotIndexRecensusProbe.java and written to the same
 * rule, which is the whole reason that file is shaped the way it is: <b>every
 * read below goes through a real JDK accessor</b> — a method whose body is JDK
 * bytecode reading the field by its own declared index — and never through the
 * native that wrote the slot. Reading a slot back through the native that wrote
 * it agrees no matter how wrong the index is.
 *
 * <p>Two further traps this file is written against, both of which have caught
 * this campaign before:
 *
 * <ul>
 *   <li><b>The vacuous assertion.</b> "The set I built is non-empty" passes
 *       against a corrupt object in several arrangements. Every set section
 *       below asserts an <i>exact</i> size and the <i>exact</i> elements, in
 *       order, and prints them as values so the four transcripts (HotSpot /
 *       CratonVM) x (before / after) diff line by line.</li>
 *   <li><b>The width repair with no observable.</b> Most of what this lane
 *       repaired was <i>wide-but-unused</i>: slots past the declared width that
 *       no site read or wrote, because the state lives in an address-keyed side
 *       table. Narrowing those cannot change any value this probe prints, and
 *       this file does not pretend otherwise — for them the observable is the
 *       row disappearing from {@code CRATONVM_DBG_LAYOUT_ALIAS=1}, and the
 *       probe's job is to prove the repair changed <b>nothing else</b>. Every
 *       such line is marked NO-CHANGE. Sections whose values genuinely differ
 *       before and after, or between HotSpot and CratonVM, are marked RED.</li>
 * </ul>
 *
 * <p>Needs no network beyond {@code 127.0.0.1} and no external files.
 */
public class OverAllocationWidthProbe {

    interface ThrowingSupplier<T> {
        T get() throws Throwable;
    }

    static String show(ThrowingSupplier<?> s) {
        try {
            return String.valueOf(s.get());
        } catch (Throwable t) {
            return t.getClass().getName() + (t.getMessage() == null ? "" : ": " + t.getMessage());
        }
    }

    static void p(String key, ThrowingSupplier<?> s) {
        System.out.println(key + "=" + show(s));
    }

    /** Contents of an Iterable as a stable, order-preserving string. */
    static String dump(Iterable<?> it) {
        StringBuilder sb = new StringBuilder("[");
        boolean first = true;
        for (Object o : it) {
            if (!first) {
                sb.append(',');
            }
            sb.append(o);
            first = false;
        }
        return sb.append(']').toString();
    }

    static TreeSet<String> base() {
        TreeSet<String> t = new TreeSet<>();
        t.add("b");
        t.add("d");
        t.add("a");
        t.add("c");
        t.add("e");
        return t;
    }

    public static void main(String[] args) throws Exception {
        treeSetViewSection();
        completableFutureSection();
        channelSection();
        fileLockSection();
        asyncServerChannelSection();
        inetSocketAddressSection();
    }

    // -- 1 -------------------------------------------------------------------
    // NO-CHANGE. Nine set-view natives allocated `java/util/TreeSet` three
    // slots wide against a class declaring one (`private transient
    // NavigableMap m`). The three indices are keys into an address-keyed side
    // table, not object slots, so the two extra slots had no reader — narrowing
    // them must move none of the values below.
    //
    // RED, and pre-existing: `m` is null on every set these natives return, so
    // the real TreeSet methods CratonVM does NOT register — `spliterator()`,
    // `clone()`, and the SequencedCollection additions — dereference null. That
    // is the null-backing-map half of the W7-49 §6.3 HashSet shape, and it is
    // NOT what an allocation width fixes. Printed so the next lane has its red.

    static void treeSetViewSection() {
        System.out.println("-- treeset views --");
        p("ts.base", () -> dump(base()));
        p("ts.base.size", () -> base().size());

        p("ts.headSet", () -> dump(base().headSet("d")));
        p("ts.headSet.size", () -> base().headSet("d").size());
        p("ts.headSet.contains.a", () -> base().headSet("d").contains("a"));
        p("ts.headSet.contains.d", () -> base().headSet("d").contains("d"));

        p("ts.headSet.inclusive", () -> dump(base().headSet("d", true)));
        p("ts.headSet.inclusive.size", () -> base().headSet("d", true).size());

        p("ts.tailSet", () -> dump(base().tailSet("c")));
        p("ts.tailSet.size", () -> base().tailSet("c").size());
        p("ts.tailSet.exclusive", () -> dump(base().tailSet("c", false)));
        p("ts.tailSet.exclusive.size", () -> base().tailSet("c", false).size());

        p("ts.subSet", () -> dump(base().subSet("b", "e")));
        p("ts.subSet.size", () -> base().subSet("b", "e").size());
        p("ts.subSet.inclusive", () -> dump(base().subSet("b", true, "e", true)));
        p("ts.subSet.inclusive.size", () -> base().subSet("b", true, "e", true).size());

        p("ts.descendingSet", () -> dump(base().descendingSet()));
        p("ts.descendingSet.size", () -> base().descendingSet().size());
        p("ts.descendingSet.first", () -> base().descendingSet().first());

        // The views are TreeSets in CratonVM's model; on HotSpot they are
        // TreeMap$KeySet / TreeSet-of-a-submap. The NAME is a discriminator, so
        // print it — a repair that changed which class comes back would show up
        // here and nowhere else.
        p("ts.headSet.class", () -> base().headSet("d").getClass().getName());
        p("ts.descendingSet.class", () -> base().descendingSet().getClass().getName());

        // Iterator, explicitly: real Set bytecode reaching the backing map on
        // HotSpot, and the shape most likely to differ if a view came back
        // narrower than its backing map expects.
        p("ts.headSet.iterator", () -> {
            List<String> got = new ArrayList<>();
            for (Iterator<String> i = base().headSet("d").iterator(); i.hasNext(); ) {
                got.add(i.next());
            }
            return dump(got);
        });

        // TreeMap.navigableKeySet — the ninth site, and a different owner.
        p("tm.navigableKeySet", () -> {
            TreeMap<String, Integer> m = new TreeMap<>();
            m.put("x", 1);
            m.put("y", 2);
            NavigableSet<String> ks = m.navigableKeySet();
            return dump(ks) + " size=" + ks.size() + " contains(y)=" + ks.contains("y");
        });

        p("cslm.keySet", () -> {
            ConcurrentSkipListMap<String, Integer> m = new ConcurrentSkipListMap<>();
            m.put("x", 1);
            m.put("y", 2);
            Set<String> ks = m.keySet();
            return dump(ks) + " size=" + ks.size() + " contains(y)=" + ks.contains("y");
        });

        // RED (pre-existing, NOT an allocation width): unregistered real
        // TreeSet bytecode dereferencing the `m` the views never populate.
        p("ts.headSet.spliterator.estimate", () -> base().headSet("d").spliterator().estimateSize());
        p("ts.headSet.stream.count", () -> base().headSet("d").stream().count());
        p("ts.headSet.equals", () -> {
            SortedSet<String> v = base().headSet("d");
            TreeSet<String> expect = new TreeSet<>(List.of("a", "b", "c"));
            return v.equals(expect) + "/" + expect.equals(v);
        });
        p("ts.headSet.toArray", () -> java.util.Arrays.toString(base().headSet("d").toArray()));
    }

    // -- 2 -------------------------------------------------------------------
    // The synthetic CompletableFuture was allocated four slots wide against a
    // class declaring two (`volatile Object result`, `volatile Completion
    // stack`). Slots 2 and 3 (`CF_FIELD_SOURCE`, `CF_FIELD_HANDLER`) had no
    // reader anywhere — NO-CHANGE.
    //
    // RED, and NOT repaired: slot 1 is `stack`, a reference, and the synthetic
    // `done` marker written there is an Int. `getNumberOfDependents()` is the
    // read that cannot be faked — its JDK body walks `stack` as a Completion
    // chain, by the JDK's own index, and CratonVM registers no native on it.
    // Same read the W7-49 §6.2 repair was proved by.

    static void completableFutureSection() {
        System.out.println("-- completablefuture --");

        p("cf.completed.class", () -> CompletableFuture.completedFuture("v").getClass().getName());
        p("cf.completed.dependents", () -> CompletableFuture.completedFuture("v").getNumberOfDependents());
        p("cf.completed.get", () -> CompletableFuture.completedFuture("v").get(5, TimeUnit.SECONDS));

        // A PENDING future is the shape every live caller of the repaired
        // helper produces. `getNumberOfDependents` on it is the load-bearing
        // line: HotSpot answers 0 for a fresh one and 1 after a dependent is
        // attached, both by walking `stack`.
        p("cf.pending.dependents.0", () -> new CompletableFuture<String>().getNumberOfDependents());
        p("cf.pending.dependents.1", () -> {
            CompletableFuture<String> f = new CompletableFuture<>();
            f.thenApply(s -> s);
            return f.getNumberOfDependents();
        });
        p("cf.pending.isDone", () -> new CompletableFuture<String>().isDone());

        // A stage derived from a pending source: the natives return their
        // synthetic pending future here.
        p("cf.derived.pending.dependents", () -> {
            CompletableFuture<String> src = new CompletableFuture<>();
            CompletableFuture<String> derived = src.thenApply(s -> s + "!");
            return derived.getNumberOfDependents();
        });
        p("cf.derived.pending.isDone", () -> {
            CompletableFuture<String> src = new CompletableFuture<>();
            return src.thenApply(s -> s + "!").isDone();
        });
        // ... and then completed, end to end. A derived future that never
        // completes is the failure a detached synthetic pending future causes.
        p("cf.derived.completes", () -> {
            CompletableFuture<String> src = new CompletableFuture<>();
            CompletableFuture<String> derived = src.thenApply(s -> s + "!");
            src.complete("ok");
            return derived.get(5, TimeUnit.SECONDS);
        });
        p("cf.derived.run.completes", () -> {
            CompletableFuture<String> src = new CompletableFuture<>();
            CompletableFuture<Void> derived = src.thenRun(() -> { });
            src.complete("ok");
            derived.get(5, TimeUnit.SECONDS);
            return derived.isDone();
        });
        p("cf.exceptional.dependents", () -> {
            CompletableFuture<String> f = new CompletableFuture<>();
            f.completeExceptionally(new IllegalStateException("boom"));
            return f.getNumberOfDependents();
        });
    }

    // -- 3 -------------------------------------------------------------------
    // NO-CHANGE for the width: SocketChannel / ServerSocketChannel were asked
    // for twelve slots against ten declared, and all twelve indices are keys
    // into the `chan_fields` side table, so slots 10 and 11 had no reader.
    //
    // RED, newly found and NOT repaired: the ONE object slot these natives do
    // touch is slot 5, cached `java.net.ServerSocket`. On the real layout slot
    // 5 is `AbstractSelectableChannel.keys`, the `SelectionKey[]`. Every read
    // below is real JDK bytecode over the declared fields — `provider()` (slot
    // 4), `isRegistered()` (`keyCount`, slot 6), `keyFor()` (walks `keys`, slot
    // 5), `isBlocking()` (`nonBlocking`, slot 9), `isOpen()` (`closed`, slot 1).

    static void channelSection() throws IOException {
        System.out.println("-- channels --");

        p("ssc.class", () -> {
            try (ServerSocketChannel c = ServerSocketChannel.open()) {
                return c.getClass().getName();
            }
        });
        p("ssc.provider", () -> {
            try (ServerSocketChannel c = ServerSocketChannel.open()) {
                Object pr = c.provider();
                return pr == null ? "null" : pr.getClass().getName();
            }
        });
        p("ssc.isOpen", () -> {
            try (ServerSocketChannel c = ServerSocketChannel.open()) {
                return c.isOpen();
            }
        });
        p("ssc.isBlocking", () -> {
            try (ServerSocketChannel c = ServerSocketChannel.open()) {
                return c.isBlocking();
            }
        });
        p("ssc.isRegistered.fresh", () -> {
            try (ServerSocketChannel c = ServerSocketChannel.open()) {
                return c.isRegistered();
            }
        });

        // The load-bearing pair. `socket()` is what writes slot 5 in CratonVM;
        // `keyFor` and `isRegistered` are JDK bytecode reading `keys` and
        // `keyCount` right after it. On HotSpot the ServerSocket cache does not
        // exist and these are unaffected by calling socket().
        p("ssc.keyFor.afterSocket", () -> {
            try (Selector sel = Selector.open();
                 ServerSocketChannel c = ServerSocketChannel.open()) {
                c.bind(new InetSocketAddress("127.0.0.1", 0));
                c.configureBlocking(false);
                SelectionKey k = c.register(sel, SelectionKey.OP_ACCEPT);
                Object ignored = c.socket();
                SelectionKey back = c.keyFor(sel);
                return "registered=" + c.isRegistered()
                        + " sameKey=" + (back == k)
                        + " socketNull=" + (ignored == null);
            }
        });
        p("ssc.socket.stable", () -> {
            try (ServerSocketChannel c = ServerSocketChannel.open()) {
                c.bind(new InetSocketAddress("127.0.0.1", 0));
                return c.socket() == c.socket();
            }
        });
        p("ssc.close.afterSocket", () -> {
            ServerSocketChannel c = ServerSocketChannel.open();
            c.bind(new InetSocketAddress("127.0.0.1", 0));
            Object ignored = c.socket();
            c.close();
            return c.isOpen();
        });

        p("sc.provider", () -> {
            try (SocketChannel c = SocketChannel.open()) {
                Object pr = c.provider();
                return pr == null ? "null" : pr.getClass().getName();
            }
        });
        p("sc.isOpen", () -> {
            try (SocketChannel c = SocketChannel.open()) {
                return c.isOpen();
            }
        });
        p("sc.isBlocking", () -> {
            try (SocketChannel c = SocketChannel.open()) {
                return c.isBlocking();
            }
        });
        p("sc.isConnected.fresh", () -> {
            try (SocketChannel c = SocketChannel.open()) {
                return c.isConnected();
            }
        });
        // An accepted child channel — the two `alloc_obj` sites that mint one.
        p("sc.accepted", () -> {
            try (ServerSocketChannel srv = ServerSocketChannel.open()) {
                srv.bind(new InetSocketAddress("127.0.0.1", 0));
                int port = ((InetSocketAddress) srv.getLocalAddress()).getPort();
                try (SocketChannel client = SocketChannel.open(new InetSocketAddress("127.0.0.1", port));
                     SocketChannel child = srv.accept()) {
                    return "childOpen=" + child.isOpen()
                            + " childConnected=" + child.isConnected()
                            + " childBlocking=" + child.isBlocking()
                            + " clientConnected=" + client.isConnected();
                }
            }
        });
    }

    // -- 4 -------------------------------------------------------------------
    // NO-CHANGE, and deliberately so. FileLock is allocated six slots wide
    // against four declared, but slots 0..3 ALIAS `channel`, `position`,
    // `size`, `shared` exactly by index and type, and 4..5 are private state
    // above the declared width — the appended-slot idiom of W7-49 §8, written
    // by hand. The four reads below are the real `final` accessors, so they
    // observe the aliased slots through the JDK's own indices: they are the
    // check that the alias is right, not that the width is.

    static void fileLockSection() throws IOException {
        System.out.println("-- filelock --");
        Path tmp = Files.createTempFile("overalloc-probe", ".bin");
        try {
            Files.write(tmp, new byte[64]);
            p("fl.acquire", () -> {
                try (FileChannel ch = FileChannel.open(tmp, StandardOpenOption.READ, StandardOpenOption.WRITE);
                     FileLock lock = ch.lock()) {
                    return "class=" + lock.getClass().getName()
                            + " position=" + lock.position()
                            + " size=" + lock.size()
                            + " shared=" + lock.isShared()
                            + " channelSame=" + (lock.channel() == ch)
                            + " valid=" + lock.isValid();
                }
            });
            p("fl.released", () -> {
                try (FileChannel ch = FileChannel.open(tmp, StandardOpenOption.READ, StandardOpenOption.WRITE)) {
                    FileLock lock = ch.lock();
                    lock.release();
                    return "valid=" + lock.isValid() + " position=" + lock.position();
                }
            });
            p("fl.tryLock.region", () -> {
                try (FileChannel ch = FileChannel.open(tmp, StandardOpenOption.READ, StandardOpenOption.WRITE)) {
                    FileLock lock = ch.tryLock(8L, 16L, false);
                    if (lock == null) {
                        return "null-lock";
                    }
                    String s = "position=" + lock.position()
                            + " size=" + lock.size()
                            + " shared=" + lock.isShared()
                            + " overlaps(0,4)=" + lock.overlaps(0L, 4L)
                            + " overlaps(8,4)=" + lock.overlaps(8L, 4L);
                    lock.release();
                    return s;
                }
            });
        } finally {
            Files.deleteIfExists(tmp);
        }
    }

    // -- 5 -------------------------------------------------------------------
    // RED, measured and NOT repaired. `AsynchronousServerSocketChannel`
    // declares exactly one instance field, `provider`, and `aio_assc_open`
    // writes an Int into slot 0. `provider()` is a real `final` accessor
    // returning that field, so it is the read that cannot be faked. Left
    // because the slot map and three registrations are shared with
    // `AsynchronousSocketChannel`, whose map another crate reads under the
    // opposite meaning — see the note on `aio_assc_open`.

    static void asyncServerChannelSection() {
        System.out.println("-- async server channel --");
        p("assc.class", () -> {
            try (AsynchronousServerSocketChannel c = AsynchronousServerSocketChannel.open()) {
                return c.getClass().getName();
            }
        });
        p("assc.provider", () -> {
            try (AsynchronousServerSocketChannel c = AsynchronousServerSocketChannel.open()) {
                Object pr = c.provider();
                return pr == null ? "null" : pr.getClass().getName();
            }
        });
        p("assc.isOpen", () -> {
            try (AsynchronousServerSocketChannel c = AsynchronousServerSocketChannel.open()) {
                return c.isOpen();
            }
        });
        p("assc.bind.localAddress", () -> {
            try (AsynchronousServerSocketChannel c = AsynchronousServerSocketChannel.open()) {
                c.bind(new InetSocketAddress("127.0.0.1", 0));
                InetSocketAddress a = (InetSocketAddress) c.getLocalAddress();
                return "host=" + a.getAddress().getHostAddress() + " portNonZero=" + (a.getPort() != 0);
            }
        });
    }

    // -- 6 -------------------------------------------------------------------
    // NO-CHANGE. The repaired site is `dc_inet_socket_address`, on the
    // DatagramChannel/DnsClient receive path, which a probe cannot reach
    // without a live DNS exchange — so this section exercises the SHAPE (real
    // accessors over the one field the class declares, `holder`) rather than
    // the repaired call site, and says so instead of claiming a green it did
    // not earn. Every accessor below is JDK bytecode dereferencing `holder`.

    static void inetSocketAddressSection() {
        System.out.println("-- inetsocketaddress --");
        p("isa.resolved", () -> {
            InetSocketAddress a = new InetSocketAddress("127.0.0.1", 5353);
            return "port=" + a.getPort()
                    + " host=" + a.getHostString()
                    + " addr=" + a.getAddress().getHostAddress()
                    + " unresolved=" + a.isUnresolved();
        });
        p("isa.equals", () -> {
            InetSocketAddress a = new InetSocketAddress("127.0.0.1", 5353);
            InetSocketAddress b = new InetSocketAddress("127.0.0.1", 5353);
            return a.equals(b) + "/" + (a.hashCode() == b.hashCode());
        });
        p("isa.toString", () -> new InetSocketAddress("127.0.0.1", 5353).toString());
        p("isa.unresolved", () -> {
            InetSocketAddress a = InetSocketAddress.createUnresolved("example.invalid", 80);
            return "unresolved=" + a.isUnresolved() + " host=" + a.getHostString() + " port=" + a.getPort();
        });
    }
}

/* ===========================================================================
 * HotSpot 25.0.3+9 ORACLE — measured on this Windows host, not guessed.
 *
 *   openjdk version "25.0.3" 2026-04-21 LTS
 *   OpenJDK Runtime Environment Temurin-25.0.3+9 (build 25.0.3+9-LTS)
 *
 * Every value below is what real JDK bytecode answers. A CratonVM run that
 * differs on a NO-CHANGE line is a regression this lane introduced; a CratonVM
 * run that differs on a RED line is the defect the section names. Two lines
 * are worth calling out because they are easy to break while "fixing" this
 * area: ssc.socket.stable=true (HotSpot caches the ServerSocket adaptor, which
 * is what CratonVM's slot-5 cache exists to match, so removing that cache
 * without a replacement flips it) and ts.headSet.class=java.util.TreeSet
 * (HotSpot really does return a TreeSet from headSet, so the class name is not
 * a discriminator between the two VMs and must not be used as one).
 *
 * -- treeset views --
 * ts.base=[a,b,c,d,e]
 * ts.base.size=5
 * ts.headSet=[a,b,c]
 * ts.headSet.size=3
 * ts.headSet.contains.a=true
 * ts.headSet.contains.d=false
 * ts.headSet.inclusive=[a,b,c,d]
 * ts.headSet.inclusive.size=4
 * ts.tailSet=[c,d,e]
 * ts.tailSet.size=3
 * ts.tailSet.exclusive=[d,e]
 * ts.tailSet.exclusive.size=2
 * ts.subSet=[b,c,d]
 * ts.subSet.size=3
 * ts.subSet.inclusive=[b,c,d,e]
 * ts.subSet.inclusive.size=4
 * ts.descendingSet=[e,d,c,b,a]
 * ts.descendingSet.size=5
 * ts.descendingSet.first=e
 * ts.headSet.class=java.util.TreeSet
 * ts.descendingSet.class=java.util.TreeSet
 * ts.headSet.iterator=[a,b,c]
 * tm.navigableKeySet=[x,y] size=2 contains(y)=true
 * cslm.keySet=[x,y] size=2 contains(y)=true
 * ts.headSet.spliterator.estimate=9223372036854775807
 * ts.headSet.stream.count=3
 * ts.headSet.equals=true/true
 * ts.headSet.toArray=[a, b, c]
 * -- completablefuture --
 * cf.completed.class=java.util.concurrent.CompletableFuture
 * cf.completed.dependents=0
 * cf.completed.get=v
 * cf.pending.dependents.0=0
 * cf.pending.dependents.1=1
 * cf.pending.isDone=false
 * cf.derived.pending.dependents=0
 * cf.derived.pending.isDone=false
 * cf.derived.completes=ok!
 * cf.derived.run.completes=true
 * cf.exceptional.dependents=0
 * -- channels --
 * ssc.class=sun.nio.ch.ServerSocketChannelImpl
 * ssc.provider=sun.nio.ch.WEPollSelectorProvider
 * ssc.isOpen=true
 * ssc.isBlocking=true
 * ssc.isRegistered.fresh=false
 * ssc.keyFor.afterSocket=registered=true sameKey=true socketNull=false
 * ssc.socket.stable=true
 * ssc.close.afterSocket=false
 * sc.provider=sun.nio.ch.WEPollSelectorProvider
 * sc.isOpen=true
 * sc.isBlocking=true
 * sc.isConnected.fresh=false
 * sc.accepted=childOpen=true childConnected=true childBlocking=true clientConnected=true
 * -- filelock --
 * fl.acquire=class=sun.nio.ch.FileLockImpl position=0 size=9223372036854775807 shared=false channelSame=true valid=true
 * fl.released=valid=false position=0
 * fl.tryLock.region=position=8 size=16 shared=false overlaps(0,4)=false overlaps(8,4)=true
 * -- async server channel --
 * assc.class=sun.nio.ch.WindowsAsynchronousServerSocketChannelImpl
 * assc.provider=sun.nio.ch.WindowsAsynchronousChannelProvider
 * assc.isOpen=true
 * assc.bind.localAddress=host=127.0.0.1 portNonZero=true
 * -- inetsocketaddress --
 * isa.resolved=port=5353 host=127.0.0.1 addr=127.0.0.1 unresolved=false
 * isa.equals=true/true
 * isa.toString=/127.0.0.1:5353
 * isa.unresolved=unresolved=true host=example.invalid port=80
 * =========================================================================== */
