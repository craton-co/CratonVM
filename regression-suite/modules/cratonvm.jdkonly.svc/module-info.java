/**
 * A real named module for the JDK-only corpus.
 *
 * Compiled by regression-suite/run.sh into regression-suite/build-modules and
 * put on the module path with --add-modules; the consumer (RJdkModule) lives in
 * the unnamed module on the class path, so this exercises boot-layer
 * construction, readability, exports vs opens, encapsulated resources and
 * module-path service providers.
 *
 * Package roles -- do not "tidy" these, each one is asserted:
 *   com.cratonvm.jdkonly.svc          exported, NOT opened
 *   com.cratonvm.jdkonly.svc.open     exported AND opened
 *   com.cratonvm.jdkonly.svc.internal neither (fully encapsulated)
 */
module cratonvm.jdkonly.svc {
    exports com.cratonvm.jdkonly.svc;
    exports com.cratonvm.jdkonly.svc.open;

    opens com.cratonvm.jdkonly.svc.open;

    provides com.cratonvm.jdkonly.svc.Greeter
            with com.cratonvm.jdkonly.svc.internal.EnGreeter,
                 com.cratonvm.jdkonly.svc.internal.FactoryGreeter;

    // The NEGATIVE half of the factory form. WrongFactory's `provider()` is
    // compiled TWICE: the `modules/` source returns Rejected (so javac accepts
    // this clause -- it refuses the illegal shape outright), and
    // `modules-overlay/` recompiles it returning Object over the same class
    // file. What the VM loads is the overlay's, so every traversal of
    // ServiceLoader.load(Rejected.class) -- iterator() AND stream() -- must
    // raise ServiceConfigurationError. Do not "fix" WrongFactory.
    provides com.cratonvm.jdkonly.svc.Rejected
            with com.cratonvm.jdkonly.svc.internal.WrongFactory;
}
