use super::*;

fn s(units: &[u16]) -> String {
    String::from_utf16(units).unwrap()
}

#[test]
fn descriptor_names_are_windows_paths() {
    assert_eq!(s(&descriptor_name("logs/2026/app.log")), r"logs\2026\app.log");
    // Characters Windows forbids are replaced per component.
    assert_eq!(s(&descriptor_name("a:b/c?d*.txt")), r"a_b\c_d_.txt");
    // Trailing dots/spaces are dropped; reserved device names are prefixed.
    assert_eq!(s(&descriptor_name("dir./CON.txt")), r"dir\_CON.txt");
    assert_eq!(s(&descriptor_name("COM1")), "_COM1");
    assert_eq!(s(&descriptor_name("COMMANDS")), "COMMANDS");
    // Unicode survives.
    assert_eq!(s(&descriptor_name("文档/报告.pdf")), r"文档\报告.pdf");
}

#[test]
fn over_long_paths_keep_folders_and_extension() {
    let long = format!("deep/{}.tar.gz", "x".repeat(400));
    let name = s(&descriptor_name(&long));
    assert!(name.encode_utf16().count() <= MAX_DESCRIPTOR_NAME);
    assert!(name.starts_with(r"deep\x"));
    assert!(name.ends_with(".gz"), "{name}");
}

#[test]
fn filetime_conversion() {
    // 1970-01-01 is 11644473600 s after 1601-01-01.
    assert_eq!(unix_to_filetime(0), 116_444_736_000_000_000);
    assert_eq!(unix_to_filetime(1), 116_444_736_010_000_000);
}

#[test]
fn chunk_reader_fills_across_chunks_and_reports_eof() {
    let mut chunks = vec![Ok(b"hello ".to_vec()), Ok(b"wor".to_vec()), Ok(b"ld".to_vec())].into_iter();
    let mut next = move || chunks.next();
    let mut reader = ChunkReader::new();

    let mut out = [0u8; 8];
    assert_eq!(reader.fill(&mut out, &mut next), Fill::Read { n: 8, eof: false });
    assert_eq!(&out, b"hello wo");

    let mut out = [0u8; 8];
    assert_eq!(reader.fill(&mut out, &mut next), Fill::Read { n: 3, eof: true });
    assert_eq!(&out[..3], b"rld");

    // Reads after the end stay at EOF without pulling again.
    assert_eq!(reader.fill(&mut out, || panic!("pulled after EOF")), Fill::Read { n: 0, eof: true });
}

fn entry(relative: &str, is_dir: bool, size: u64) -> ExportEntry {
    ExportEntry {
        remote: format!("/srv/{relative}"),
        relative: relative.to_string(),
        is_dir,
        size,
        mtime: 0,
    }
}

#[test]
fn transfer_row_is_named_after_the_dragged_items() {
    // One folder: its name, total of every file inside.
    let folder = [
        entry("logs", true, 0),
        entry("logs/a.log", false, 100),
        entry("logs/old", true, 0),
        entry("logs/old/b.log", false, 50),
    ];
    assert_eq!(transfer_label(&folder), ("logs".to_string(), 150));
    // Several items: "first +N".
    let many = [entry("a.txt", false, 1), entry("b.txt", false, 2), entry("c", true, 0)];
    assert_eq!(transfer_label(&many), ("a.txt +2".to_string(), 3));
}

#[test]
fn drag_transfer_reports_one_row_with_a_single_final_state() {
    use crate::sftp::DragTransfer;
    use std::sync::atomic::Ordering;

    // Nothing requested: no row at all, not even a final one.
    let idle = DragTransfer::new("d0".into(), "x".into(), 10);
    assert_eq!(idle.final_update(), None);

    // Normal copy: progress, then Done with the full size.
    let ok = DragTransfer::new("d1".into(), "logs".into(), 150);
    assert_eq!(ok.progress_update(true), Some(0));
    ok.done.fetch_add(150, Ordering::Relaxed);
    assert_eq!(ok.final_update(), Some((1, 150, String::new())));
    // After the final state nothing else is emitted.
    assert_eq!(ok.progress_update(true), None);
    assert_eq!(ok.final_update(), None);

    // Explorer stopped early: Cancelled.
    let cut = DragTransfer::new("d2".into(), "big.iso".into(), 1000);
    cut.progress_update(true);
    cut.done.fetch_add(400, Ordering::Relaxed);
    cut.interrupted.store(true, Ordering::Relaxed);
    assert_eq!(cut.final_update(), Some((4, 400, String::new())));

    // A failure wins and keeps the first message.
    let bad = DragTransfer::new("d3".into(), "secret".into(), 10);
    bad.progress_update(true);
    bad.fail("secret: permission denied".into());
    bad.fail("later error".into());
    bad.interrupted.store(true, Ordering::Relaxed);
    assert_eq!(bad.final_update(), Some((2, 0, "secret: permission denied".into())));

    // Progress is throttled between forced updates.
    let busy = DragTransfer::new("d4".into(), "f".into(), 10);
    assert!(busy.progress_update(false).is_some());
    assert_eq!(busy.progress_update(false), None);
}

#[test]
fn chunk_reader_surfaces_stream_errors() {
    let mut chunks = vec![Ok(b"ab".to_vec()), Err("permission denied".to_string())].into_iter();
    let mut reader = ChunkReader::new();
    let mut out = [0u8; 4];
    assert_eq!(
        reader.fill(&mut out, || chunks.next()),
        Fill::Failed("permission denied".to_string())
    );
}
