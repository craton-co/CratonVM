// JAVA21+
package cratonvm;

/**
 * Phase 84.3: Sealed class verification — permitted subclasses load correctly.
 */
public class SealedVerify {

    sealed interface Animal permits Dog, Cat {}
    record Dog(String name) implements Animal {}
    record Cat(String name) implements Animal {}

    // 84.3: Permitted subclass loads successfully
    public static int testPermittedSubclassLoads() {
        Animal a = new Dog("Rex");
        return a instanceof Dog ? 1 : 0;  // 1
    }

    // 84.3: Sealed interface with multiple permitted implementors
    public static int testMultiplePermitted() {
        Animal a = new Cat("Whiskers");
        Animal b = new Dog("Buddy");
        int result = 0;
        if (a instanceof Cat) result += 1;
        if (b instanceof Dog) result += 2;
        return result;  // 3
    }

    // 84.3: Switch with default on sealed type
    public static int testSealedWithDefault() {
        Animal a = new Dog("Fido");
        return switch (a) {
            case Dog d  -> 10;
            case Cat c  -> 20;
            default     -> -1;
        };
    }
}
