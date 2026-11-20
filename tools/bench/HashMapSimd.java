import jdk.incubator.vector.IntVector;

/** Flat integer put/get comparison; deliberately separate from java.util.HashMap. */
public final class HashMapSimd {
    // shortcut: put/get only, add tombstones before introducing deletion workloads.
    static final class IntMap {
        static final int LANES = 16;

        private int[] keys = new int[LANES];
        private int[] values = new int[keys.length];
        private int[] occupied = new int[keys.length / LANES];
        private int size;
        private final boolean simd;

        IntMap(boolean simd) { this.simd = simd; }

        private int find(int key) {
            int group = (key ^ (key >>> 16)) & (occupied.length - 1);
            int full = (1 << LANES) - 1;
            IntVector query = simd ? IntVector.broadcast(IntVector.SPECIES_512, key) : null;
            for (;;) {
                int bits = occupied[group];
                int start = group * LANES;
                int matches = 0;
                if (simd) {
                    matches = (int) IntVector.fromArray(IntVector.SPECIES_512, keys, start).eq(query).toLong() & bits;
                } else {
                    for (int remaining=bits; remaining!=0; remaining &= remaining-1) {
                        int lane = Integer.numberOfTrailingZeros(remaining);
                        if (keys[start+lane] == key) return start+lane;
                    }
                }
                if (matches != 0) return start + Integer.numberOfTrailingZeros(matches);
                if (bits != full) return ~(start + Integer.numberOfTrailingZeros(~bits & full));
                group = (group + 1) & (occupied.length - 1);
            }
        }

        void put(int key, int value) {
            int slot = find(key);
            if (slot >= 0) { values[slot] = value; return; }
            if (size >= keys.length - keys.length/4) {
                grow();
                slot = find(key);
            }
            slot = ~slot;
            keys[slot] = key;
            values[slot] = value;
            occupied[slot / LANES] |= 1 << (slot % LANES);
            size++;
        }

        int getOrDefault(int key, int absent) {
            int slot = find(key);
            return slot < 0 ? absent : values[slot];
        }

        int size() { return size; }

        private void grow() {
            int[] oldKeys = keys, oldValues = values, oldOccupied = occupied;
            if (keys.length >= (1 << 29)) throw new IllegalStateException("integer map capacity limit");
            int[] newKeys = new int[oldKeys.length * 2];
            int[] newValues = new int[newKeys.length];
            int[] newOccupied = new int[newKeys.length / LANES];
            keys = newKeys;
            values = newValues;
            occupied = newOccupied;
            size = 0;
            for (int g=0; g<oldOccupied.length; g++) {
                int bits = oldOccupied[g];
                while (bits != 0) {
                    int slot = g * LANES + Integer.numberOfTrailingZeros(bits);
                    put(oldKeys[slot], oldValues[slot]);
                    bits &= bits - 1;
                }
            }
        }
    }

    static long run(int n, boolean simd) {
        IntMap map = new IntMap(simd);
        for (int i=0; i<n; i++) map.put(i, i*31+7);
        long sum = 0;
        for (int i=0; i<n; i++) sum += map.getOrDefault(i, 0);
        return sum;
    }

    public static void main(String[] args) {
        int n = args.length == 0 ? 1_000_000 : Integer.parseInt(args[0]);
        if (n < 0) throw new IllegalArgumentException("negative entry count");
        if (args.length > 1 && !args[1].equals("scalar") && !args[1].equals("simd"))
            throw new IllegalArgumentException("mode must be scalar or simd");
        boolean simd = args.length < 2 || args[1].equals("simd");
        long start = System.nanoTime();
        long sum = run(n, simd);
        long millis = (System.nanoTime() - start) / 1_000_000;
        System.err.println("flat-map group bits: 512");
        System.out.println("HashMapFlat " + (simd ? "simd" : "scalar") + " " + n + " put/get: " + millis + " ms  [" + sum + "]");
    }
}
