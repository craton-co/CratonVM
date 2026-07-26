import org.hamcrest.Matcher;
import org.hamcrest.Matchers;
import org.hamcrest.StringDescription;

import java.util.ArrayList;
import java.util.List;

// ES-HAMCREST.1 repro: after the Elasticsearch FFM change, Hamcrest's
// equality matcher reportedly mis-evaluates under JIT while the real test
// body still runs. No Elasticsearch checkout is available on this host, so
// this stresses the real hamcrest-core/hamcrest-library matchers directly
// (IsEqual, containsString, hasSize, contains, allOf) across many varied
// inputs, matching both true and false cases, to cross JIT invocation
// thresholds on the matcher codegen itself.
public class HamcrestProbe {
    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        int failures = 0;

        for (int i = 0; i < iterations; i++) {
            String s = "value-" + i;
            String sameValue = "value-" + i;
            String otherValue = "value-" + (i + 1);

            Matcher<String> eqMatcher = Matchers.equalTo(sameValue);
            boolean shouldMatch = eqMatcher.matches(s);
            boolean shouldNotMatch = eqMatcher.matches(otherValue);
            if (!shouldMatch) {
                failures++;
                if (failures <= 5) {
                    System.out.println("EQUALTO-FALSE-NEGATIVE at i=" + i + " s=" + s);
                }
            }
            if (shouldNotMatch) {
                failures++;
                if (failures <= 5) {
                    System.out.println("EQUALTO-FALSE-POSITIVE at i=" + i + " s=" + s + " other=" + otherValue);
                }
            }

            boolean containsOk = Matchers.containsString("val").matches(s)
                    && !Matchers.containsString("zzz").matches(s);
            if (!containsOk) {
                failures++;
                if (failures <= 5) {
                    System.out.println("CONTAINSSTRING MISMATCH at i=" + i);
                }
            }

            List<Integer> list = new ArrayList<>();
            for (int j = 0; j < (i % 5) + 1; j++) {
                list.add(j);
            }
            boolean sizeOk = Matchers.hasSize(list.size()).matches(list);
            if (!sizeOk) {
                failures++;
                if (failures <= 5) {
                    System.out.println("HASSIZE MISMATCH at i=" + i + " size=" + list.size());
                }
            }

            @SuppressWarnings("unchecked")
            Matcher<String> combined = Matchers.allOf(
                    Matchers.startsWith("value-"),
                    Matchers.not(Matchers.equalTo(otherValue)));
            boolean combinedOk = combined.matches(s);
            if (!combinedOk) {
                failures++;
                if (failures <= 5) {
                    System.out.println("ALLOF MISMATCH at i=" + i);
                }
            }

            if (i % 2000 == 0) {
                System.out.println("progress i=" + i);
                System.out.flush();
            }
        }
        System.out.println("DONE iterations=" + iterations + " failures=" + failures);
        if (failures > 0) {
            System.exit(1);
        }
    }
}
