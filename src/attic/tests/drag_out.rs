use super::*;

fn s(units: &[u16]) -> String {
    String::from_utf16(units).unwrap()
}

#[test]
fn descriptor_names_are_windows_paths() {
    assert_eq!(
        s(&descriptor_name("logs/2026/app.log")),
        r"logs\2026\app.log"
    );
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
    let mut chunks = vec![
        Ok(b"hello ".to_vec()),
        Ok(b"wor".to_vec()),
        Ok(b"ld".to_vec()),
    ]
    .into_iter();
    let mut next = move || chunks.next();
    let mut reader = ChunkReader::new();

    let mut out = [0u8; 8];
    assert_eq!(
        reader.fill(&mut out, &mut next),
        Fill::Read { n: 8, eof: false }
    );
    assert_eq!(&out, b"hello wo");

    let mut out = [0u8; 8];
    assert_eq!(
        reader.fill(&mut out, &mut next),
        Fill::Read { n: 3, eof: true }
    );
    assert_eq!(&out[..3], b"rld");

    // Reads after the end stay at EOF without pulling again.
    assert_eq!(
        reader.fill(&mut out, || panic!("pulled after EOF")),
        Fill::Read { n: 0, eof: true }
    );
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
    let many = [
        entry("a.txt", false, 1),
        entry("b.txt", false, 2),
        entry("c", true, 0),
    ];
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
    assert_eq!(
        bad.final_update(),
        Some((2, 0, "secret: permission denied".into()))
    );

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

// --- File promises (macOS) ---------------------------------------------------

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("meatshell-drag-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn export(remote: &str, relative: &str, is_dir: bool) -> ExportEntry {
    ExportEntry {
        remote: remote.into(),
        relative: relative.into(),
        is_dir,
        size: if is_dir { 0 } else { remote.len() as u64 },
        mtime: 1_700_000_000,
    }
}

fn walk() -> Vec<ExportEntry> {
    vec![
        export("/srv/logs", "logs", true),
        export("/srv/logs/2026", "logs/2026", true),
        export("/srv/logs/2026/app.log", "logs/2026/app.log", false),
        export("/srv/logs/empty", "logs/empty", true),
        export("/srv/notes.txt", "notes.txt", false),
    ]
}

#[test]
fn dragged_items_are_named_after_their_last_component() {
    let item = |remote: &str| DragItem {
        remote: remote.into(),
        is_dir: false,
    };
    assert_eq!(item("/srv/logs").name(), "logs");
    assert_eq!(item("/srv/logs/").name(), "logs");
    assert_eq!(item("/").name(), "download");
}

#[test]
fn promise_paths_stay_inside_the_chosen_location() {
    let dest = std::path::Path::new("dest");
    assert_eq!(
        local_path_under(dest, "logs", "logs"),
        Some(dest.to_path_buf())
    );
    assert_eq!(
        local_path_under(dest, "logs", "logs/2026/app.log"),
        Some(dest.join("2026").join("app.log"))
    );
    // Another dragged item, or a sibling whose name merely starts the same.
    assert_eq!(local_path_under(dest, "logs", "notes.txt"), None);
    assert_eq!(local_path_under(dest, "logs", "logs-old/a"), None);
    assert_eq!(local_path_under(dest, "logs", "logs/../escape"), None);
}

#[test]
fn materialise_creates_a_folder_tree_or_a_single_file() {
    let dir = TempDir::new();
    let dest = dir.0.join("logs 2"); // Finder may rename on a clash
    let mut copied = Vec::new();
    materialise(&dest, "logs", &walk(), |e, file| {
        use std::io::Write;
        copied.push(e.remote.clone());
        file.write_all(e.remote.as_bytes())
            .map_err(|e| e.to_string())
    })
    .unwrap();
    assert_eq!(copied, ["/srv/logs/2026/app.log"]);
    let log = dest.join("2026").join("app.log");
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "/srv/logs/2026/app.log"
    );
    assert!(dest.join("empty").is_dir());
    let modified = std::fs::metadata(&log).unwrap().modified().unwrap();
    assert_eq!(
        modified
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        1_700_000_000
    );

    let file = dir.0.join("notes.txt");
    materialise(&file, "notes.txt", &walk(), |_, _| Ok(())).unwrap();
    assert!(file.is_file());
    assert!(materialise(&dir.0.join("x"), "missing", &walk(), |_, _| Ok(())).is_err());
}

#[test]
fn a_failed_file_is_removed_and_the_error_returned() {
    let dir = TempDir::new();
    let dest = dir.0.join("notes.txt");
    let result = materialise(&dest, "notes.txt", &walk(), |_, _| {
        Err("connection lost".into())
    });
    assert_eq!(result, Err("connection lost".to_string()));
    assert!(!dest.exists());
}

/// A stand-in SFTP worker: answers the walk, streams each file's remote
/// path as its contents (or fails `/srv/broken`), reports finished drags.
fn fake_worker(
    entries: Vec<ExportEntry>,
) -> (
    UnboundedSender<SftpCommand>,
    std::sync::mpsc::Receiver<(String, bool)>,
) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        while let Some(command) = rx.blocking_recv() {
            match command {
                SftpCommand::WalkForExport { reply, .. } => {
                    let _ = reply.send(Ok(entries.clone()));
                }
                SftpCommand::StreamFile { remote, chunks, .. } => {
                    let chunk = if remote == "/srv/broken" {
                        Err("read failed".to_string())
                    } else {
                        Ok(remote.into_bytes())
                    };
                    let _ = chunks.blocking_send(chunk);
                }
                SftpCommand::FinishDragTransfer(transfer) => {
                    let interrupted = transfer.interrupted.load(Ordering::Relaxed);
                    let _ = done_tx.send((transfer.name.clone(), interrupted));
                }
                _ => {}
            }
        }
    });
    (tx, done_rx)
}

#[test]
fn promise_drag_writes_each_item_and_reports_once_after_the_last() {
    let dir = TempDir::new();
    let (commands, finished) = fake_worker(walk());
    let items = [
        DragItem {
            remote: "/srv/logs".into(),
            is_dir: true,
        },
        DragItem {
            remote: "/srv/notes.txt".into(),
            is_dir: false,
        },
    ];
    let drag = PromiseDrag::start(commands, &items).unwrap();
    drag.write(&items[1], &dir.0.join("notes.txt")).unwrap();
    assert!(finished.try_recv().is_err(), "one promise is still open");
    drag.write(&items[0], &dir.0.join("logs")).unwrap();
    let wait = std::time::Duration::from_secs(5);
    assert_eq!(
        finished.recv_timeout(wait).unwrap(),
        ("logs +1".to_string(), false)
    );
    assert_eq!(
        std::fs::read_to_string(dir.0.join("logs/2026/app.log")).unwrap(),
        "/srv/logs/2026/app.log"
    );
    // The worker channel is released: later calls do nothing.
    drag.finish();
    assert!(finished
        .recv_timeout(std::time::Duration::from_millis(200))
        .is_err());
}

#[test]
fn promise_drag_failures_mark_the_row_interrupted() {
    let dir = TempDir::new();
    let (commands, finished) = fake_worker(vec![export("/srv/broken", "broken", false)]);
    let item = DragItem {
        remote: "/srv/broken".into(),
        is_dir: false,
    };
    let drag = PromiseDrag::start(commands, std::slice::from_ref(&item)).unwrap();
    assert_eq!(
        drag.write(&item, &dir.0.join("broken")),
        Err("read failed".to_string())
    );
    assert!(!dir.0.join("broken").exists());
    let wait = std::time::Duration::from_secs(5);
    assert_eq!(
        finished.recv_timeout(wait).unwrap(),
        ("broken".to_string(), true)
    );
}

#[test]
fn a_cancelled_promise_drag_reports_nothing() {
    let (commands, finished) = fake_worker(walk());
    let item = DragItem {
        remote: "/srv/notes.txt".into(),
        is_dir: false,
    };
    let drag = PromiseDrag::start(commands, std::slice::from_ref(&item)).unwrap();
    drag.finish(); // nothing dropped
    assert!(finished
        .recv_timeout(std::time::Duration::from_millis(200))
        .is_err());
    assert!(drag.write(&item, std::path::Path::new("unused")).is_err());
}
