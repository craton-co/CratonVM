import org.junit.jupiter.api.Test;
import static org.junit.jupiter.api.Assertions.*;
public class SmokeTest {
    @Test void addition() { assertEquals(4, 2 + 2); }
    @Test void strings() { assertTrue("hello".startsWith("hel")); }
    @Test void arrays() { int[] a = {3,1,2}; java.util.Arrays.sort(a); assertArrayEquals(new int[]{1,2,3}, a); }
}
