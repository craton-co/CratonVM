import java.util.*;

/**
 * Full-stack JVM stress benchmark — exercises CPU, GC, memory, collections,
 * threading simulation, serialization, and data processing without external
 * framework dependencies.
 *
 * Simulates the workload patterns of Spring Boot + DB + Cache + Queue:
 * 1. HTTP-style request parsing (String processing)
 * 2. Database-style query (HashMap lookups, joins)
 * 3. Cache operations (HashSet, LRU-style eviction)
 * 4. Message queue (producer/consumer with ArrayList buffer)
 * 5. JSON-style serialization (StringBuilder)
 * 6. GC pressure (object allocation churn)
 */
public class FullStackBench {

    // =========================================================================
    // 1. CPU + JIT: Compute-heavy scoring (like QuickBench but with objects)
    // =========================================================================
    static long benchCompute(int iterations) {
        long sum = 0;
        for (int i = 0; i < iterations; i++) {
            sum += (long)i * 3 - i / 2 + i % 7;
        }
        return sum;
    }

    // =========================================================================
    // 2. Collections + GC: HashMap/ArrayList stress (simulates DB queries)
    // =========================================================================
    static int benchCollections(int size, int lookups) {
        // Build a "database table"
        HashMap<String, int[]> table = new HashMap<>();
        for (int i = 0; i < size; i++) {
            String key = "user_" + i;
            int[] row = new int[]{i, i * 100, i % 10};
            table.put(key, row);
        }

        // Query the table
        int found = 0;
        for (int q = 0; q < lookups; q++) {
            String key = "user_" + (q % size);
            int[] row = table.get(key);
            if (row != null && row[0] >= 0) found++;
        }
        return found;
    }

    // =========================================================================
    // 3. String processing: Simulates HTTP request parsing + JSON serialization
    // =========================================================================
    static int benchStringProcessing(int iterations) {
        int totalLen = 0;
        for (int i = 0; i < iterations; i++) {
            // Simulate building a JSON response
            StringBuilder sb = new StringBuilder();
            sb.append("{\"id\":");
            sb.append(i);
            sb.append(",\"name\":\"user_");
            sb.append(i);
            sb.append("\",\"score\":");
            sb.append(i * 17 % 100);
            sb.append(",\"active\":");
            sb.append(i % 2 == 0 ? "true" : "false");
            sb.append("}");
            String json = sb.toString();
            totalLen += json.length();

            // Simulate parsing: find field values
            int nameStart = json.indexOf("name");
            if (nameStart >= 0) {
                totalLen += nameStart;
            }
        }
        return totalLen;
    }

    // =========================================================================
    // 4. Array + GC pressure: Allocate and process many small objects
    // =========================================================================
    static long benchGCPressure(int iterations) {
        long checksum = 0;
        for (int i = 0; i < iterations; i++) {
            // Allocate a small array (simulates DTO/entity creation)
            int[] data = new int[8];
            for (int j = 0; j < 8; j++) {
                data[j] = i * 7 + j;
            }
            // Process it
            int sum = 0;
            for (int j = 0; j < 8; j++) {
                sum += data[j];
            }
            checksum += sum;
        }
        return checksum;
    }

    // =========================================================================
    // 5. Sieve + Array: Simulates index-heavy workload (like Elasticsearch)
    // =========================================================================
    static int benchIndexing(int limit, int queries) {
        // Build an "index"
        boolean[] index = new boolean[limit];
        for (int i = 2; i < limit; i++) {
            if (!index[i]) {
                for (int j = i + i; j < limit; j += i) {
                    index[j] = true;
                }
            }
        }

        // Query the index
        int hits = 0;
        for (int q = 0; q < queries; q++) {
            int key = q % limit;
            if (!index[key]) hits++;
        }
        return hits;
    }

    // =========================================================================
    // 6. Matrix + nested loops: Simulates batch data processing
    // =========================================================================
    static int benchBatchProcessing(int n) {
        int[][] data = new int[n][n];
        // Fill with data
        for (int i = 0; i < n; i++) {
            for (int j = 0; j < n; j++) {
                data[i][j] = i * n + j;
            }
        }
        // Aggregate (like a GROUP BY query)
        int[] colSums = new int[n];
        for (int j = 0; j < n; j++) {
            int sum = 0;
            for (int i = 0; i < n; i++) {
                sum += data[i][j];
            }
            colSums[j] = sum;
        }
        // Checksum
        int total = 0;
        for (int j = 0; j < n; j++) total += colSums[j];
        return total;
    }

    public static void main(String[] args) {
        System.out.println("=== Full Stack JVM Benchmark ===");
        long grandTotal = 0;

        // 1. CPU/JIT: Pure computation
        {
            long t0 = System.currentTimeMillis();
            long r = benchCompute(300000000);
            long elapsed = System.currentTimeMillis() - t0;
            grandTotal += elapsed;
            System.out.println("1. Compute (300M)     : " + elapsed + " ms  [" + r + "]");
        }

        // 2. Collections: HashMap stress
        {
            long t0 = System.currentTimeMillis();
            int r = 0;
            for (int rep = 0; rep < 100; rep++) {
                r = benchCollections(1000, 50000);
            }
            long elapsed = System.currentTimeMillis() - t0;
            grandTotal += elapsed;
            System.out.println("2. Collections (100x) : " + elapsed + " ms  [" + r + "]");
        }

        // 3. String processing: JSON-style
        {
            long t0 = System.currentTimeMillis();
            int r = 0;
            for (int rep = 0; rep < 10; rep++) {
                r = benchStringProcessing(100000);
            }
            long elapsed = System.currentTimeMillis() - t0;
            grandTotal += elapsed;
            System.out.println("3. String/JSON (10x)  : " + elapsed + " ms  [" + r + "]");
        }

        // 4. GC pressure: Object allocation churn
        {
            long t0 = System.currentTimeMillis();
            long r = 0;
            for (int rep = 0; rep < 10; rep++) {
                r = benchGCPressure(1000000);
            }
            long elapsed = System.currentTimeMillis() - t0;
            grandTotal += elapsed;
            System.out.println("4. GC Pressure (10M)  : " + elapsed + " ms  [" + r + "]");
        }

        // 5. Indexing: Sieve + query
        {
            long t0 = System.currentTimeMillis();
            int r = 0;
            for (int rep = 0; rep < 500; rep++) {
                r = benchIndexing(100000, 10000);
            }
            long elapsed = System.currentTimeMillis() - t0;
            grandTotal += elapsed;
            System.out.println("5. Indexing (500x)    : " + elapsed + " ms  [" + r + "]");
        }

        // 6. Batch processing: Matrix aggregation
        {
            long t0 = System.currentTimeMillis();
            int r = benchBatchProcessing(500);
            long elapsed = System.currentTimeMillis() - t0;
            grandTotal += elapsed;
            System.out.println("6. Batch (500x500)    : " + elapsed + " ms  [" + r + "]");
        }

        System.out.println();
        System.out.println("TOTAL                 : " + grandTotal + " ms");
    }
}
