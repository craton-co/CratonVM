import org.junit.jupiter.api.Test;
import static org.junit.jupiter.api.Assertions.*;

/** JUnit5 (jupiter-engine) smoke test: one pass, one fail. */
public class Smoke5 {
    @Test void passes() { assertEquals(4, 2 + 2); }
    @Test void fails()  { assertEquals(1, 2, "boom"); }
}
