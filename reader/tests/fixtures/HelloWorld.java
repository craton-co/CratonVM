// Test fixture for reader::class_reader's constant-pool Utf8 interning
// test, which `include_bytes!`es HelloWorld.class beside this file at
// compile time.
//
// It lives here rather than in `test_classes/` because `.gitignore`
// ignores `test_classes/**/*.class`. A 2026-06-17 remediation added the
// fixture there and recorded it as committed; git dropped it silently,
// and `cargo test -p cratonvm-reader` has failed to COMPILE in every
// fresh checkout since — while passing on any machine where someone had
// run `javac` in that directory. `<crate>/tests/fixtures/` is where the
// rest of the workspace keeps its binary fixtures, and no ignore rule
// covers it.
//
// Compiled to major version 52 (Java 8). Nothing in the test depends on
// the version, but a fixture whose bytes change under you is a fixture
// that can only be debugged twice, so regenerate it deliberately:
//
//     javac --release 8 -d reader/tests/fixtures \
//         reader/tests/fixtures/HelloWorld.java
//
// The test needs a real constant pool with at least three Utf8 entries
// and at least one duplicated string. Any class this size has both.
public class HelloWorld {
    public static void main(String[] args) {
        System.out.println("Hello, World!");
    }
}
