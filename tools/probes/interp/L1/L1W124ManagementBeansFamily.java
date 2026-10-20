// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 124, lane L1 -- the flip-rule probe and bench for
// the `JdkManagementBeans` compat family (alignment bit 97,
// `CRATONVM_COMPAT_JDK_MANAGEMENT_BEANS`, default off): with the row on,
// `--compatible` runs the JDK's own constructors, `getName`, `isValid` and
// `getType` of `sun.management.GarbageCollectorImpl`, `MemoryManagerImpl` and
// `MemoryPoolImpl`, as `--jdk-only` does. Page:
// docs/internal/proposals/interpreter/i74-L1-proposal-route-to-own-bytecode-natives-become-family-rows-20261107.md.
//
// Covers: Java-built beans of all three classes (reflection, so the run needs
// --add-opens java.management/sun.management=ALL-UNNAMED) and their pure-Java
// readers (name, validity, type, threshold support, the threshold getters'
// UnsupportedOperationException, ObjectName); and the VM's beans from
// ManagementFactory by their invariants only (names non-empty and unique,
// valid, both memory types present, ObjectName shape, the pool <-> manager
// name graph consistent, every collector a manager), since the names are the
// collector's. No ACC_NATIVE method of a Java-built bean is called (those
// natives are genuine and not in the row). TIME lines: getName / isValid /
// getType on a Java-built pool and on a VM pool, and the pool constructor.
//
// Must be SAME in all three arms (TIME lines filtered):
//   javac -d out L1W124ManagementBeansFamily.java
//   java --add-opens java.management/sun.management=ALL-UNNAMED -cp out L1W124ManagementBeansFamily
//   cratonvm --jdk-only --add-opens java.management/sun.management=ALL-UNNAMED -cp out L1W124ManagementBeansFamily
//   cratonvm --compatible --add-opens java.management/sun.management=ALL-UNNAMED -cp out L1W124ManagementBeansFamily
//   CRATONVM_COMPAT_JDK_MANAGEMENT_BEANS=1 cratonvm --compatible --add-opens java.management/sun.management=ALL-UNNAMED -cp out L1W124ManagementBeansFamily
//   (each also with --nojit for the interpreter's TIME rows)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint identical; TIME lines filtered)
// pool heap name=p-heap valid=true type=Heap memory usageSupported=true collSupported=false
// pool heap usageThreshold=10
// pool heap collectionUsageThreshold -> java.lang.UnsupportedOperationException: [CollectionUsage threshold is not supported]
// pool heap objectName=java.lang:type=MemoryPool,name=p-heap
// pool nonheap name=p-nonheap valid=true type=Non-heap memory usageSupported=false collSupported=true
// pool nonheap usageThreshold -> java.lang.UnsupportedOperationException: [Usage threshold is not supported]
// pool nonheap collectionUsageThreshold=0
// pool nonheap objectName=java.lang:type=MemoryPool,name=p-nonheap
// pool null-name name=null valid=true type=Heap memory
// manager name=m-1 valid=true objectName=java.lang:type=MemoryManager,name=m-1
// manager null-name name=null valid=true
// gc name=g-1 valid=true objectName=java.lang:type=GarbageCollector,name=g-1 isManager=true
// gc classes sun.management.GarbageCollectorImpl/sun.management.MemoryManagerImpl/sun.management.MemoryPoolImpl
// vm pools nonempty=true names-ok=true unique=true valid=true heap=true nonheap=true objectNames=true
// vm managers nonempty=true names-ok=true unique=true valid=true objectNames=true
// vm gcs nonempty=true names-ok=true valid=true allManagers=true objectNames=true
// vm graph pool->manager=true manager->pool=true
// vm repeat-stable=true
// done
// HOTSPOT_EXPECTED_END

import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryManagerMXBean;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryType;
import java.lang.reflect.Constructor;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashSet;
import java.util.List;
import java.util.Set;

public class L1W124ManagementBeansFamily {
    static volatile long sink;

    static Constructor<?> poolCtor;
    static Constructor<?> managerCtor;
    static Constructor<?> gcCtor;

    static MemoryPoolMXBean pool(String name, boolean heap, long usage, long coll) throws Exception {
        return (MemoryPoolMXBean) poolCtor.newInstance(name, heap, usage, coll);
    }

    static String thrown(Throwable t) {
        return t.getClass().getName() + ": [" + t.getMessage() + "]";
    }

    static void describePool(String label, MemoryPoolMXBean p) {
        System.out.println("pool " + label + " name=" + p.getName() + " valid=" + p.isValid()
                + " type=" + p.getType() + " usageSupported=" + p.isUsageThresholdSupported()
                + " collSupported=" + p.isCollectionUsageThresholdSupported());
        try {
            System.out.println("pool " + label + " usageThreshold=" + p.getUsageThreshold());
        } catch (Throwable t) {
            System.out.println("pool " + label + " usageThreshold -> " + thrown(t));
        }
        try {
            System.out.println("pool " + label + " collectionUsageThreshold=" + p.getCollectionUsageThreshold());
        } catch (Throwable t) {
            System.out.println("pool " + label + " collectionUsageThreshold -> " + thrown(t));
        }
        System.out.println("pool " + label + " objectName=" + p.getObjectName());
    }

    static boolean namesOk(List<String> names) {
        for (String n : names) {
            if (n == null || n.isEmpty()) {
                return false;
            }
        }
        return true;
    }

    public static void main(String[] args) throws Exception {
        Class<?> poolClass = Class.forName("sun.management.MemoryPoolImpl");
        Class<?> managerClass = Class.forName("sun.management.MemoryManagerImpl");
        Class<?> gcClass = Class.forName("sun.management.GarbageCollectorImpl");
        poolCtor = poolClass.getDeclaredConstructor(String.class, boolean.class, long.class, long.class);
        poolCtor.setAccessible(true);
        managerCtor = managerClass.getDeclaredConstructor(String.class);
        managerCtor.setAccessible(true);
        gcCtor = gcClass.getDeclaredConstructor(String.class);
        gcCtor.setAccessible(true);

        // Java-built beans: the routed constructors and the retired readers.
        describePool("heap", pool("p-heap", true, 10L, -1L));
        describePool("nonheap", pool("p-nonheap", false, -1L, 0L));
        MemoryPoolMXBean nullPool = pool(null, true, -1L, -1L);
        System.out.println("pool null-name name=" + nullPool.getName() + " valid=" + nullPool.isValid()
                + " type=" + nullPool.getType());

        MemoryManagerMXBean m = (MemoryManagerMXBean) managerCtor.newInstance("m-1");
        System.out.println("manager name=" + m.getName() + " valid=" + m.isValid()
                + " objectName=" + m.getObjectName());
        MemoryManagerMXBean mNull = (MemoryManagerMXBean) managerCtor.newInstance((Object) null);
        System.out.println("manager null-name name=" + mNull.getName() + " valid=" + mNull.isValid());

        Object g = gcCtor.newInstance("g-1");
        GarbageCollectorMXBean gc = (GarbageCollectorMXBean) g;
        System.out.println("gc name=" + gc.getName() + " valid=" + gc.isValid()
                + " objectName=" + gc.getObjectName() + " isManager=" + (g instanceof MemoryManagerMXBean));
        System.out.println("gc classes " + gcClass.getName() + "/" + gcClass.getSuperclass().getName()
                + "/" + poolClass.getName());

        // The VM's beans, by their invariants.
        List<MemoryPoolMXBean> pools = ManagementFactory.getMemoryPoolMXBeans();
        List<String> poolNames = new ArrayList<>();
        boolean poolsValid = true, heap = false, nonheap = false, poolObjectNames = true;
        for (MemoryPoolMXBean p : pools) {
            poolNames.add(p.getName());
            poolsValid &= p.isValid();
            heap |= p.getType() == MemoryType.HEAP;
            nonheap |= p.getType() == MemoryType.NON_HEAP;
            poolObjectNames &= ("java.lang:type=MemoryPool,name=" + p.getName())
                    .equals(p.getObjectName().toString());
        }
        System.out.println("vm pools nonempty=" + !pools.isEmpty() + " names-ok=" + namesOk(poolNames)
                + " unique=" + (new HashSet<>(poolNames).size() == poolNames.size()) + " valid=" + poolsValid
                + " heap=" + heap + " nonheap=" + nonheap + " objectNames=" + poolObjectNames);

        List<MemoryManagerMXBean> managers = ManagementFactory.getMemoryManagerMXBeans();
        List<String> managerNames = new ArrayList<>();
        boolean managersValid = true, managerObjectNames = true;
        for (MemoryManagerMXBean mm : managers) {
            managerNames.add(mm.getName());
            managersValid &= mm.isValid();
            String type = (mm instanceof GarbageCollectorMXBean) ? "GarbageCollector" : "MemoryManager";
            managerObjectNames &= ("java.lang:type=" + type + ",name=" + mm.getName())
                    .equals(mm.getObjectName().toString());
        }
        System.out.println("vm managers nonempty=" + !managers.isEmpty() + " names-ok=" + namesOk(managerNames)
                + " unique=" + (new HashSet<>(managerNames).size() == managerNames.size())
                + " valid=" + managersValid + " objectNames=" + managerObjectNames);

        List<GarbageCollectorMXBean> gcs = ManagementFactory.getGarbageCollectorMXBeans();
        List<String> gcNames = new ArrayList<>();
        boolean gcsValid = true, gcObjectNames = true;
        for (GarbageCollectorMXBean b : gcs) {
            gcNames.add(b.getName());
            gcsValid &= b.isValid();
            gcObjectNames &= ("java.lang:type=GarbageCollector,name=" + b.getName())
                    .equals(b.getObjectName().toString());
        }
        System.out.println("vm gcs nonempty=" + !gcs.isEmpty() + " names-ok=" + namesOk(gcNames)
                + " valid=" + gcsValid + " allManagers=" + managerNames.containsAll(gcNames)
                + " objectNames=" + gcObjectNames);

        Set<String> poolSet = new HashSet<>(poolNames);
        Set<String> managerSet = new HashSet<>(managerNames);
        boolean poolToManager = true, managerToPool = true;
        for (MemoryPoolMXBean p : pools) {
            poolToManager &= managerSet.containsAll(Arrays.asList(p.getMemoryManagerNames()));
        }
        for (MemoryManagerMXBean mm : managers) {
            managerToPool &= poolSet.containsAll(Arrays.asList(mm.getMemoryPoolNames()));
        }
        System.out.println("vm graph pool->manager=" + poolToManager + " manager->pool=" + managerToPool);

        List<String> again = new ArrayList<>();
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            again.add(p.getName());
        }
        System.out.println("vm repeat-stable=" + again.equals(poolNames));

        // TIME rows.
        MemoryPoolMXBean built = pool("bench", true, 1L, 1L);
        MemoryPoolMXBean vmPool = pools.get(0);
        for (int rep = 0; rep < 3; rep++) {
            time("java-pool-readers", built, 500_000);
            time("vm-pool-readers", vmPool, 500_000);
            long t0 = System.nanoTime();
            for (int i = 0; i < 20_000; i++) {
                sink += pool("c", (i & 1) == 0, i, -1L).getName().length();
            }
            System.out.println("TIME pool-ctor rep=" + rep + " ns/op=" + (System.nanoTime() - t0) / 20_000);
        }
        System.out.println("done");
    }

    static void time(String label, MemoryPoolMXBean p, int n) {
        long t0 = System.nanoTime();
        long acc = 0;
        for (int i = 0; i < n; i++) {
            acc += p.getName().length();
            acc += p.isValid() ? 1 : 0;
            acc += p.getType().ordinal();
        }
        sink += acc;
        System.out.println("TIME " + label + " ns/op=" + (System.nanoTime() - t0) / n);
    }
}
