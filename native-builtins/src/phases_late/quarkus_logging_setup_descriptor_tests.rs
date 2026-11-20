    use super::{logging_setup_arg_plan, LoggingSetupArg, LoggingSetupRefusal};

    /// The shape the mirror used to have written down as a literal: SIX
    /// `Ljava/util/List;` parameters. Quarkus removed it, and because the
    /// literal was the only thing that said which arity to call, every call
    /// raised `NoSuchMethodError` (found 2026-08-17, fixed 2026-08-13 in
    /// `70248949c`). Pinned so this shape stays SERVED, not special-cased.
    const SIX_LIST_SHAPE: &str = "(Lio/quarkus/runtime/logging/DiscoveredLogComponents;\
Ljava/util/Map;ZLio/quarkus/runtime/RuntimeValue;\
Ljava/util/List;Ljava/util/List;Ljava/util/List;Ljava/util/List;Ljava/util/List;Ljava/util/List;\
Lio/quarkus/runtime/RuntimeValue;Lio/quarkus/runtime/LaunchMode;Z)\
Lio/quarkus/runtime/shutdown/ShutdownListener;";

    /// The shape quarkus 999-SNAPSHOT actually ships: SEVEN lists, the seventh
    /// being the per-`NamedHandlerType` formatter map.
    const SEVEN_LIST_SHAPE: &str = "(Lio/quarkus/runtime/logging/DiscoveredLogComponents;\
Ljava/util/Map;ZLio/quarkus/runtime/RuntimeValue;\
Ljava/util/List;Ljava/util/List;Ljava/util/List;Ljava/util/List;Ljava/util/List;Ljava/util/List;\
Ljava/util/List;Lio/quarkus/runtime/RuntimeValue;Lio/quarkus/runtime/LaunchMode;Z)\
Lio/quarkus/runtime/shutdown/ShutdownListener;";

    fn plan(desc: &str) -> Vec<LoggingSetupArg> {
        logging_setup_arg_plan(desc).expect("this shape must be modellable")
    }

    #[test]
    fn both_recorder_shapes_are_served_with_the_right_arity() {
        assert_eq!(plan(SIX_LIST_SHAPE).len(), 13);
        assert_eq!(plan(SEVEN_LIST_SHAPE).len(), 14);
    }

    #[test]
    fn every_list_slot_is_an_empty_list_on_both_shapes() {
        for desc in [SIX_LIST_SHAPE, SEVEN_LIST_SHAPE] {
            let lists = plan(desc)
                .iter()
                .filter(|a| **a == LoggingSetupArg::EmptyList)
                .count();
            let declared = desc.matches("Ljava/util/List;").count();
            assert_eq!(
                lists, declared,
                "every declared List must be filled with an empty list ({desc})"
            );
        }
    }

    /// The handler `RuntimeValue` takes `aconst_null` and the supplier
    /// `RuntimeValue` — the LAST one — takes the caller's value. Getting this
    /// backwards is invisible in an arity check, so pin the positions.
    #[test]
    fn only_the_last_runtime_value_carries_the_supplier() {
        for desc in [SIX_LIST_SHAPE, SEVEN_LIST_SHAPE] {
            let p = plan(desc);
            let suppliers: Vec<usize> = p
                .iter()
                .enumerate()
                .filter(|(_, a)| **a == LoggingSetupArg::SupplierRuntimeValue)
                .map(|(i, _)| i)
                .collect();
            assert_eq!(suppliers.len(), 1, "exactly one supplier slot ({desc})");
            assert_eq!(
                suppliers[0],
                p.len() - 3,
                "the supplier is the RuntimeValue before LaunchMode ({desc})"
            );
            assert_eq!(
                p[3],
                LoggingSetupArg::Null,
                "the handler RuntimeValue is null"
            );
        }
    }

    #[test]
    fn the_fixed_slots_are_where_the_bytecode_puts_them() {
        let p = plan(SEVEN_LIST_SHAPE);
        assert_eq!(p[0], LoggingSetupArg::Components);
        assert_eq!(p[1], LoggingSetupArg::EmptyMap);
        assert_eq!(p[2], LoggingSetupArg::False);
        assert_eq!(p[p.len() - 2], LoggingSetupArg::LaunchMode);
        assert_eq!(p[p.len() - 1], LoggingSetupArg::False);
    }

    /// A primitive the mirror has no value for must REFUSE and say which slot,
    /// not guess. Refusing is what keeps a moved signature from being called
    /// with a fabricated argument.
    #[test]
    fn an_unfillable_primitive_refuses_and_names_the_slot() {
        let moved = "(Lio/quarkus/runtime/logging/DiscoveredLogComponents;I)\
Lio/quarkus/runtime/shutdown/ShutdownListener;";
        assert_eq!(
            logging_setup_arg_plan(moved),
            Err(LoggingSetupRefusal::UnfillableParameter(1, "I".to_string()))
        );
    }

    #[test]
    fn a_non_descriptor_refuses_rather_than_panicking() {
        assert_eq!(
            logging_setup_arg_plan("not a descriptor"),
            Err(LoggingSetupRefusal::UnparseableDescriptor)
        );
    }
