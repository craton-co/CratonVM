import org.junit.Test;
import static org.junit.Assert.*;

/** JUnit4 (vintage-engine) smoke test: one pass, one fail. */
public class Smoke4 {
    @Test public void passes() { assertEquals(4, 2 + 2); }
    @Test public void fails()  { assertEquals("boom", 1, 2); }
}
