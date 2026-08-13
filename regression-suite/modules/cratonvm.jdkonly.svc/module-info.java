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

    // The other illegal factory shape, and the one javac CAN express: a
    // provider() that returns null. It must fail at Provider.get(), not while
    // the wrapper is built -- see Nulled.
    provides com.cratonvm.jdkonly.svc.Nulled
            with com.cratonvm.jdkonly.svc.internal.NullProvider;

    // The CONSTRUCTOR-form negatives -- the half of ServiceLoader.loadProvider
    // that applies once findStaticProviderMethod has answered null. Both
    // providers are compiled TWICE for the same reason WrongFactory is: javac
    // enforces BOTH rules on a `provides` clause, so neither illegal shape can
    // be written here directly. modules-overlay/ carries what actually runs.
    //
    //   Unsub  <- NotSubProvider  implements nothing        => "not a subtype"
    //   Ctored <- HiddenCtor      private no-arg ctor       => "Unable to get
    //                                                          public no-arg
    //                                                          constructor"
    //
    // That javac enforcement is also the blast-radius argument for arming the
    // two gates: every `provides` clause in the JDK's own boot modules is javac
    // output, so no boot-module provider can be in the set either gate refuses.
    // Do not "fix" either provider.
    provides com.cratonvm.jdkonly.svc.Unsub
            with com.cratonvm.jdkonly.svc.internal.NotSubProvider;

    provides com.cratonvm.jdkonly.svc.Ctored
            with com.cratonvm.jdkonly.svc.internal.HiddenCtor;
}
