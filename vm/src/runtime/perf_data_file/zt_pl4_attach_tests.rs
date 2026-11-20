    use super::*;

    /// HotSpot's `get_namespace_pid` takes the second pid of `NSpid:`.
    #[test]
    fn namespace_pid_is_the_second_nspid() {
        let status = "Name:	java
Tgid:	4242
NSpid:	4242	7
PPid:	1
";
        assert_eq!(namespace_pid_from_status(status), 7);
        assert_eq!(namespace_pid_from_status("NSpid:	4242
"), -1);
        assert_eq!(namespace_pid_from_status("Name:	java
"), -1);
    }

    /// HotSpot's `filename_to_pid`: digits only.
    #[test]
    fn filename_pid_takes_digits_only() {
        assert_eq!(filename_pid("1234"), Some(1234));
        assert_eq!(filename_pid("1234.tmp"), None);
        assert_eq!(filename_pid("x1"), None);
        assert_eq!(filename_pid(""), None);
    }

    /// The teardown unmaps every recorded view once and empties the record.
    #[cfg(unix)]
    #[test]
    fn teardown_releases_every_recorded_view() {
        // SAFETY: an anonymous private page; the result is checked.
        let page = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(page, libc::MAP_FAILED);
        let mut views = vec![(page as usize, 4096)]; // Cast: the mapping's address
        assert_eq!(release_foreign_views_at_teardown(&mut views), 1);
        assert!(views.is_empty());
        assert_eq!(release_foreign_views_at_teardown(&mut views), 0);
    }
