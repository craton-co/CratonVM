use super::*;
use crate::event::PeerId;

#[test]
fn empty_clipboard() {
    let mgr = ClipboardManager::new();
    assert!(mgr.get_text(ClipboardKind::System).is_none());
    assert!(mgr.get_text(ClipboardKind::Selection).is_none());
    assert!(mgr.available_flavors(ClipboardKind::System).is_empty());
}

#[test]
fn set_and_get_text_system() {
    let mut mgr = ClipboardManager::new();
    mgr.set_text(ClipboardKind::System, "hello".to_string(), Some(PeerId(1)));

    assert_eq!(mgr.get_text(ClipboardKind::System), Some("hello"));
    // Selection should be unaffected.
    assert!(mgr.get_text(ClipboardKind::Selection).is_none());
}

#[test]
fn set_and_get_text_selection() {
    let mut mgr = ClipboardManager::new();
    mgr.set_text(ClipboardKind::Selection, "world".to_string(), None);
    assert_eq!(mgr.get_text(ClipboardKind::Selection), Some("world"));
    assert!(mgr.get_text(ClipboardKind::System).is_none());
}

#[test]
fn text_round_trip_with_both_flavors() {
    let mut mgr = ClipboardManager::new();
    mgr.set_text(ClipboardKind::System, "test".to_string(), None);

    assert!(mgr.has_flavor(ClipboardKind::System, &DataFlavor::StringFlavor));
    assert!(mgr.has_flavor(ClipboardKind::System, &DataFlavor::PlainTextFlavor));
    assert!(!mgr.has_flavor(ClipboardKind::System, &DataFlavor::ImageFlavor));
}

#[test]
fn overwrite_clipboard() {
    let mut mgr = ClipboardManager::new();
    mgr.set_text(ClipboardKind::System, "first".to_string(), None);
    mgr.set_text(ClipboardKind::System, "second".to_string(), None);
    assert_eq!(mgr.get_text(ClipboardKind::System), Some("second"));
}

#[test]
fn set_contents_with_image() {
    let mut mgr = ClipboardManager::new();
    let mut contents = ClipboardContents::new();
    contents.set(
        DataFlavor::ImageFlavor,
        ClipboardData::Image {
            pixels: vec![0xFF000000; 4],
            width: 2,
            height: 2,
        },
    );
    contents.set_owner(Some(PeerId(5)));

    mgr.set_contents(ClipboardKind::System, contents);

    assert!(mgr.has_flavor(ClipboardKind::System, &DataFlavor::ImageFlavor));
    assert!(!mgr.has_flavor(ClipboardKind::System, &DataFlavor::StringFlavor));

    let c = mgr.get_contents(ClipboardKind::System);
    assert_eq!(c.owner(), Some(PeerId(5)));

    if let Some(ClipboardData::Image {
        pixels,
        width,
        height,
    }) = c.get(&DataFlavor::ImageFlavor)
    {
        assert_eq!(*width, 2);
        assert_eq!(*height, 2);
        assert_eq!(pixels.len(), 4);
    } else {
        panic!("expected Image data");
    }
}

#[test]
fn file_list_flavor() {
    let mut mgr = ClipboardManager::new();
    let mut contents = ClipboardContents::new();
    contents.set(
        DataFlavor::FileListFlavor,
        ClipboardData::FileList(vec!["/tmp/a.txt".to_string(), "/tmp/b.txt".to_string()]),
    );
    mgr.set_contents(ClipboardKind::System, contents);

    assert!(mgr.has_flavor(ClipboardKind::System, &DataFlavor::FileListFlavor));
    let c = mgr.get_contents(ClipboardKind::System);
    if let Some(ClipboardData::FileList(files)) = c.get(&DataFlavor::FileListFlavor) {
        assert_eq!(files.len(), 2);
        assert_eq!(files[0], "/tmp/a.txt");
    } else {
        panic!("expected FileList");
    }
}

#[test]
fn custom_flavor() {
    let mut mgr = ClipboardManager::new();
    let flavor = DataFlavor::Custom("application/json".to_string());
    let mut contents = ClipboardContents::new();
    contents.set(
        flavor.clone(),
        ClipboardData::Raw(b"{\"key\":\"value\"}".to_vec()),
    );
    mgr.set_contents(ClipboardKind::System, contents);

    assert!(mgr.has_flavor(ClipboardKind::System, &flavor));
    let flavors = mgr.available_flavors(ClipboardKind::System);
    assert_eq!(flavors.len(), 1);
}

#[test]
fn available_flavors_lists_all() {
    let mut contents = ClipboardContents::new();
    contents.set(DataFlavor::StringFlavor, ClipboardData::Text("hi".into()));
    contents.set(
        DataFlavor::ImageFlavor,
        ClipboardData::Image {
            pixels: vec![],
            width: 0,
            height: 0,
        },
    );

    let flavors = contents.available_flavors();
    assert_eq!(flavors.len(), 2);
    assert!(flavors.contains(&DataFlavor::StringFlavor));
    assert!(flavors.contains(&DataFlavor::ImageFlavor));
}

#[test]
fn clipboard_contents_default_is_empty() {
    let c = ClipboardContents::default();
    assert!(c.get_text().is_none());
    assert!(c.available_flavors().is_empty());
    assert_eq!(c.owner(), None);
}

#[test]
fn global_singleton_accessible() {
    let clip = get_clipboard();
    let mut mgr = clip.lock();
    mgr.set_text(ClipboardKind::System, "global-test".to_string(), None);
    assert_eq!(mgr.get_text(ClipboardKind::System), Some("global-test"));
}
