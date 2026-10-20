// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 122, lane L4
// (`i39-L4-proposal-one-record-layout-table-per-class`, items 1-2;
// `CRATONVM_RECORD_LAYOUT_TABLE`, default off): the Hibernate-shaped record
// hash the page names. Records keyed in a HashMap, with nested-record,
// `boolean`, `String`, enum, `Long` and `float` components, hashed and compared
// millions of times. stdout is deterministic (fixed hashes, the lookup count
// and the found values' sum); the timing goes to stderr.
//
// The switch must not change stdout. With it on, a record hash reads each
// class's fast-path answer from the per-VM table instead of the class-manager
// lock (once per record and once per nested reference component).
//
// Run: javac -d out L4W122RecordLayoutTableBench.java
//      cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L4W122RecordLayoutTableBench
//      CRATONVM_RECORD_LAYOUT_TABLE=1 cratonvm ... (A/B: interleave, take medians, pin to P-cores)
//
// Expected HotSpot 25.0.3 output (default and -Xint; default 200 rounds, an
// argument sets the count):
//   bool-false: 1237
//   bool-true: 1231
//   nested: true
//   nan: 2143289344
//   map-size: 4096
//   found: 819200 sum: 1677312000
// (with argument 20: `found: 81920 sum: 167731200`). HotSpot's stderr line is
// ~115 ns/lookup with the JIT and ~18 us under -Xint on this host.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags, no argument; stdout only, the
// timing is on stderr; added by the wave-123 L9 review, re-run 2026-10-19)
// bool-false: 1237
// bool-true: 1231
// nested: true
// nan: 2143289344
// map-size: 4096
// found: 819200 sum: 1677312000
// HOTSPOT_EXPECTED_END
import java.util.HashMap;
import java.util.Map;

public class L4W122RecordLayoutTableBench {
    enum Kind { INSERT, UPDATE, DELETE }

    record Z(boolean b) {}

    record F(float v) {}

    record Inner(String table, Kind kind, boolean batched) {}

    record Key(Inner inner, Long id, int shard, boolean dirty) {}

    public static void main(String[] args) {
        System.out.println("bool-false: " + new Z(false).hashCode());
        System.out.println("bool-true: " + new Z(true).hashCode());
        System.out.println("nested: " + new Key(new Inner("t", Kind.UPDATE, true), 5L, 3, false)
                .equals(new Key(new Inner("t", Kind.UPDATE, true), 5L, 3, false)));
        System.out.println("nan: " + new F(Float.intBitsToFloat(0x7F800001)).hashCode());

        Key[] keys = new Key[4096];
        Kind[] kinds = Kind.values();
        for (int i = 0; i < keys.length; i++) {
            keys[i] = new Key(new Inner("table" + (i % 37), kinds[i % 3], (i & 4) != 0),
                    (long) (i * 7919), i % 11, (i & 1) != 0);
        }
        Map<Key, Integer> map = new HashMap<>();
        for (int i = 0; i < keys.length; i++) {
            map.put(keys[i], i);
        }
        System.out.println("map-size: " + map.size());

        long found = 0;
        long valueSum = 0;
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 200;
        long t0 = System.nanoTime();
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < keys.length; i++) {
                Key probe = new Key(new Inner(keys[i].inner().table(), keys[i].inner().kind(),
                        keys[i].inner().batched()), keys[i].id(), keys[i].shard(), keys[i].dirty());
                Integer v = map.get(probe);
                if (v != null && probe.equals(keys[v])) {
                    found++;
                    valueSum += v;
                }
            }
        }
        long t1 = System.nanoTime();
        // Enum hashes are identity hashes, so no hash goes to stdout here.
        System.out.println("found: " + found + " sum: " + valueSum);
        System.err.printf("record-hash: %d lookups, %.1f ns/lookup%n",
                (long) rounds * keys.length, (t1 - t0) / (double) ((long) rounds * keys.length));
    }
}
