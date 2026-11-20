    use super::*;

    fn snapshot(id: u64, owned: Option<Vec<String>>, no_java_thread: bool) -> ThreadSnapshot {
        ThreadSnapshot {
            id,
            name: format!("t{id}"),
            daemon: false,
            priority: 5,
            state: ThreadState::Runnable,
            stack_frames: Vec::new(),
            lock_info: None,
            blocked_by: None,
            waiting_on: None,
            wait_kind: None,
            dump: ThreadDumpFacts {
                java_tid: Some(id),
                no_java_thread,
                owned_synchronizers: owned,
                ..Default::default()
            },
        }
    }

    #[test]
    fn zt28mf_the_section_follows_each_java_thread() {
        let header = ThreadDumpHeader::from_properties(&[]);
        let threads = [
            snapshot(
                1,
                Some(vec![
                    "<0x0000000629c16e28> (a java.util.concurrent.locks.ReentrantLock$NonfairSync)"
                        .to_string(),
                ]),
                false,
            ),
            snapshot(2, Some(Vec::new()), false),
            snapshot(3, None, false),
        ];
        let dump = JstackProcessor::generate_thread_dump_with(&header, &threads);
        assert!(dump.contains(
            "

   Locked ownable synchronizers:
	- <0x0000000629c16e28> (a java.util.concurrent.locks.ReentrantLock$NonfairSync)

"
        ));
        assert!(dump.contains("   Locked ownable synchronizers:
	- None

"));
        assert_eq!(dump.matches("Locked ownable synchronizers:").count(), 2, "none for t3");
    }

    #[test]
    fn zt28mf_thread_print_l_keeps_the_last_value() {
        let args = |list: &[&str]| list.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        assert!(!thread_print_locks_arg(&args(&[])));
        assert!(thread_print_locks_arg(&args(&["-l"])));
        assert!(thread_print_locks_arg(&args(&["-e", "-l=true"])));
        assert!(!thread_print_locks_arg(&args(&["-l", "-l=false"])));
    }
