// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.HashMap;
import java.util.Map;
import java.util.stream.Collectors;
import java.util.stream.IntStream;

/**
 * Prices the exact pipeline GPULlama3's `Vocabulary` constructor uses to
 * index a tokenizer:
 *
 *     IntStream.range(0, n).boxed().collect(Collectors.toMap(i -> tok[i], i -> i))
 *
 * A Llama-3.2 vocabulary is ~128K entries. A `--nojit` stack profile of
 * CratonVM loading that model spent 58% of its samples inside this
 * constructor's lambda, so it is worth knowing whether the cost is the
 * stream machinery or the map itself. The plain `HashMap` loop below is the
 * control: same work, same hashing, no stream pipeline. If CratonVM's gap
 * to HotSpot is much larger on the stream arm than on the loop arm, the
 * pipeline is the defect rather than the data structure.
 */
public class StreamToMapProbe {
    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 128256;
        String[] tok = new String[n];
        for (int i = 0; i < n; i++) {
            tok[i] = "tok" + i;
        }

        // Control: the same map built with an ordinary loop.
        long bestLoop = Long.MAX_VALUE;
        int loopSize = 0;
        for (int pass = 0; pass < 3; pass++) {
            long t0 = System.nanoTime();
            Map<String, Integer> m = new HashMap<>();
            for (int i = 0; i < n; i++) {
                m.put(tok[i], i);
            }
            long dt = System.nanoTime() - t0;
            loopSize = m.size();
            if (dt < bestLoop) bestLoop = dt;
        }

        // The shape Vocabulary actually uses.
        long bestStream = Long.MAX_VALUE;
        int streamSize = 0;
        for (int pass = 0; pass < 3; pass++) {
            final String[] t = tok;
            long t0 = System.nanoTime();
            Map<String, Integer> m = IntStream.range(0, n)
                    .boxed()
                    .collect(Collectors.toMap(i -> t[i], i -> i));
            long dt = System.nanoTime() - t0;
            streamSize = m.size();
            if (dt < bestStream) bestStream = dt;
        }

        System.out.println("STREAMTOMAP n=" + n
                + " loop_ms=" + (bestLoop / 1_000_000.0)
                + " stream_ms=" + (bestStream / 1_000_000.0)
                + " stream_over_loop=" + String.format("%.2f", bestStream / (double) bestLoop)
                + " loop_size=" + loopSize + " stream_size=" + streamSize);
    }
}
