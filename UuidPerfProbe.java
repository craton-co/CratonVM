import java.util.UUID;
import org.hibernate.id.uuid.UuidValueGenerator;
import org.hibernate.id.uuid.UuidVersion6Strategy;
import org.hibernate.id.uuid.UuidVersion7Strategy;
import org.hibernate.engine.spi.SharedSessionContractImplementor;

import static org.assertj.core.api.Assertions.assertThat;
import static org.mockito.Mockito.mock;

public final class UuidPerfProbe {
    public static void main(String[] args) {
        int count = Integer.parseInt(args[0]);
        boolean assertj = args.length > 2 && args[2].equals("assertj");
        boolean mockSession = args.length > 3 && args[3].equals("mock");
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
        for (int i = 0; i < count; i++) values[i] = generator.generateUuid(session);
        long generated = System.nanoTime();
        int ordered = 0;
        for (int i = 1; i < count; i++) {
            if (assertj) {
                assertThat(values[i].toString()).isGreaterThan(values[i - 1].toString());
                assertThat(values[i]).isGreaterThan(values[i - 1]);
                ordered++;
            }
            else if (values[i].toString().compareTo(values[i - 1].toString()) > 0
                    && values[i].compareTo(values[i - 1]) > 0) {
                ordered++;
            }
        }
        long compared = System.nanoTime();
        System.out.println("@@PERF generator=" + args[1] + " assertj=" + assertj + " count=" + count
                + " generated_ms=" + ((generated - started) / 1_000_000)
                + " compared_ms=" + ((compared - generated) / 1_000_000)
                + " ordered=" + ordered);
    }
}
