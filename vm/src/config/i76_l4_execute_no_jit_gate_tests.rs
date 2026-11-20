    use super::VmConfig;


    /// Interpreter round i6 wave 120, lane L7:
    /// `CRATONVM_LOADER_DICTIONARY_BEFORE_DRIVE` is read once per VM,
    /// `runtime_flag_on`; unset, it is off.
    #[test]
    fn the_loader_dictionary_before_drive_switch_follows_a_truthy_word_and_defaults_off() {
        for (word, on) in [
            (None, false),
            (Some("0"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_LOADER_DICTIONARY_BEFORE_DRIVE", word)],
                || {
                    assert_eq!(VmConfig::default().loader_dictionary_before_drive, on, "{word:?}");
                },
            );
        }
    }

    /// Interpreter round i6 wave 129, lane L7:
    /// `CRATONVM_LOADER_DRIVE_UNDER_LOADER_LOCK` is read once per VM,
    /// `runtime_flag_default_on`; unset, it is on, and `0` turns it off.
/// Interpreter round i6 wave 119, lane L7:
    /// `CRATONVM_USER_LOADER_RESOURCE_BYTECODE` is read once per VM,
    /// `runtime_flag_default_on`; unset, it is on (wave 134, user decision).

    /// Interpreter round i6 wave 132, lane L5b:
    /// `CRATONVM_USER_LOADER_RESOURCE_BYTECODE_COMPILED` is read once per VM,
    /// `runtime_flag_on`; unset, it is off.
    #[test]
    fn the_user_loader_resource_bytecode_compiled_switch_follows_a_truthy_word_and_defaults_off() {
        for (word, on) in [
            (None, false),
            (Some("0"), false),
            (Some("1"), true),
            (Some("true"), true),
        ] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_USER_LOADER_RESOURCE_BYTECODE_COMPILED", word)],
                || {
                    assert_eq!(
                        VmConfig::default().user_loader_resource_bytecode_compiled,
                        on,
                        "{word:?}"
                    );                },
            );
        }
    }
