package lib;

// What lib-3.4.jar ships: package-private, so app.Main's `new lib.Hidden()`
// (compiled against the public src/lib/lib/Hidden.java) is an
// IllegalAccessError at resolution.
class Hidden {
    public Hidden() {
    }
}
