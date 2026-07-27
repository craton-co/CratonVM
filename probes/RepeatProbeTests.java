import org.junit.jupiter.api.RepeatedTest;
import org.junit.jupiter.api.Test;

public class RepeatProbeTests {
    @Test void plain() { }
    @RepeatedTest(3) void repeated() { }
}
