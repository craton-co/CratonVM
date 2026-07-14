import java.util.UUID;
import org.hibernate.id.uuid.UuidValueGenerator;
import org.hibernate.id.uuid.UuidVersion6Strategy;
import org.hibernate.id.uuid.UuidVersion7Strategy;
import org.hibernate.engine.spi.SharedSessionContractImplementor;
import org.assertj.core.internal.StandardComparisonStrategy;

import static org.assertj.core.api.Assertions.assertThat;
import static org.mockito.Mockito.mock;

public final class UuidPerfProbe {
    public static void main(String[] args) {
        int count = Integer.parseInt(args[0]);
        boolean assertj = args.length > 2 && args[2].equals("assertj");
        boolean assertjStringOnly = args.length > 2 && args[2].equals("assertjstringonly");
        boolean assertjUuidOnly = args.length > 2 && args[2].equals("assertjuuidonly");
        boolean preStringsUuidAssert = args.length > 2 && args[2].equals("prestringsuuidassert");
        boolean assertjStringUuidPlain = args.length > 2 && args[2].equals("assertjstringuuidplain");
        boolean stringsDiscard = args.length > 2 && args[2].equals("stringsdiscard");
        boolean preStringsUuidFactory = args.length > 2 && args[2].equals("prestringsuuidfactory");
        boolean preLiteralUuidAssert = args.length > 2 && args[2].equals("preliteraluuidassert");
        boolean preStringsInterface = args.length > 2 && args[2].equals("prestringsinterface");
        boolean preStringsUuidNotNull = args.length > 2 && args[2].equals("prestringsuuidnotnull");
        boolean preStringsObjectArray = args.length > 2 && args[2].equals("prestringsobjectarray");
        boolean preStringsStrategy = args.length > 2 && args[2].equals("prestringsstrategy");
        boolean preStringsUuidAssertBreakdown = args.length > 2 && args[2].equals("prestringsuuidassertbreakdown");
        boolean mockSession = args.length > 3 && args[3].equals("mock");
        boolean gcEvery = args.length > 4 && args[4].equals("gc");
        boolean gcBeforeCompare = args.length > 4 && args[4].equals("gcbefore");
        if (args.length > 2 && args[2].equals("assertjstrings")) {
            long started = System.nanoTime();
            for (int i = 1; i <= count; i++) {
                assertThat("00000000-0000-0000-0000-000000000001")
                        .isGreaterThan("00000000-0000-0000-0000-000000000000");
                if (i % 100_000 == 0) {
                    System.out.println(marker("assertjstrings", i, started));
                }
            }
            System.out.println("@@PERF assertjstrings count=" + count
                    + " elapsed_ms=" + ((System.nanoTime() - started) / 1_000_000));
            return;
        }
        UuidValueGenerator generator = args[1].equals("v7")
                ? UuidVersion7Strategy.INSTANCE : UuidVersion6Strategy.INSTANCE;
        SharedSessionContractImplementor session = null;
        if (mockSession) {
            System.out.println("@@PERF before_mock");
            session = mock(SharedSessionContractImplementor.class);
            System.out.println("@@PERF after_mock");
        }
        UUID[] values = new UUID[count];
        long started = System.nanoTime();
        for (int i = 0; i < count; i++) {
            values[i] = generator.generateUuid(session);
            if ((i + 1) % 100_000 == 0) {
                System.out.println(marker("generated", i + 1, started));
                if (gcEvery) System.gc();
            }
        }
        long generated = System.nanoTime();
        if (gcBeforeCompare) System.gc();
        int ordered = 0;
        long stringNs = 0;
        long assertionNs = 0;
        for (int i = 1; i < count; i++) {
            if (preStringsUuidAssertBreakdown) {
                long beforeStrings = System.nanoTime();
                values[i].toString();
                values[i - 1].toString();
                stringNs += System.nanoTime() - beforeStrings;
                long beforeAssertion = System.nanoTime();
                assertThat(values[i]).isGreaterThan(values[i - 1]);
                assertionNs += System.nanoTime() - beforeAssertion;
                ordered++;
                if ((i + 1) % 10_000 == 0) {
                    System.out.println("@@BREAKDOWN compared=" + (i + 1)
                            + " strings_ms=" + (stringNs / 1_000_000)
                            + " assertion_ms=" + (assertionNs / 1_000_000));
                    stringNs = 0;
                    assertionNs = 0;
                }
            }
            else if (preStringsStrategy) {
                values[i].toString();
                values[i - 1].toString();
                if (StandardComparisonStrategy.instance().isGreaterThan(values[i], values[i - 1])) ordered++;
            }
            else if (preStringsObjectArray) {
                values[i].toString();
                values[i - 1].toString();
                Object[] ignored = { values[i] };
                if (ignored.length == 1 && values[i].compareTo(values[i - 1]) > 0) ordered++;
            }
            else if (preStringsUuidNotNull) {
                values[i].toString();
                values[i - 1].toString();
                assertThat(values[i]).isNotNull();
                ordered++;
            }
            else if (preStringsInterface) {
                values[i].toString();
                values[i - 1].toString();
                Comparable<UUID> actual = values[i];
                if (actual.compareTo(values[i - 1]) > 0) ordered++;
            }
            else if (preLiteralUuidAssert) {
                new String("00000000-0000-0000-0000-000000000001");
                new String("00000000-0000-0000-0000-000000000000");
                assertThat(values[i]).isGreaterThan(values[i - 1]);
                ordered++;
            }
            else if (stringsDiscard) {
                values[i].toString();
                values[i - 1].toString();
                ordered++;
            }
            else if (preStringsUuidFactory) {
                values[i].toString();
                values[i - 1].toString();
                assertThat(values[i]);
                ordered++;
            }
            else if (preStringsUuidAssert) {
                values[i].toString();
                values[i - 1].toString();
                assertThat(values[i]).isGreaterThan(values[i - 1]);
                ordered++;
            }
            else if (assertjStringUuidPlain) {
                assertThat(values[i].toString()).isGreaterThan(values[i - 1].toString());
                if (values[i].compareTo(values[i - 1]) > 0) ordered++;
            }
            else if (assertj || assertjStringOnly || assertjUuidOnly) {
                if (!assertjUuidOnly) {
                assertThat(values[i].toString()).isGreaterThan(values[i - 1].toString());
                }
                if (!assertjStringOnly) {
                assertThat(values[i]).isGreaterThan(values[i - 1]);
                }
                ordered++;
            }
            else if (values[i].toString().compareTo(values[i - 1].toString()) > 0
                    && values[i].compareTo(values[i - 1]) > 0) {
                ordered++;
            }
            if ((i + 1) % 100_000 == 0) {
                System.out.println(marker("compared", i + 1, started));
                if (gcEvery) System.gc();
            }
        }
        long compared = System.nanoTime();
        System.out.println("@@PERF generator=" + args[1] + " assertj=" + args[2] + " count=" + count
                + " generated_ms=" + ((generated - started) / 1_000_000)
                + " compared_ms=" + ((compared - generated) / 1_000_000)
                + " ordered=" + ordered);
    }

    private static String marker(String phase, int count, long started) {
        Runtime runtime = Runtime.getRuntime();
        return "@@PERF " + phase + "=" + count
                + " elapsed_ms=" + ((System.nanoTime() - started) / 1_000_000)
                + " free=" + runtime.freeMemory()
                + " total=" + runtime.totalMemory();
    }
}
