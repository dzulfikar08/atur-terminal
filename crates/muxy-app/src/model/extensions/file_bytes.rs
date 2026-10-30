//! `files.readBytes` and `files.writeBytes`: whole binary files, moved in
//! pieces when the server can, so one file may hold up to 64 MiB.

use std::future::Future;

use base64::{Engine, engine::general_purpose::STANDARD};
use muxy_protocol::{FilesAction, FilesReply, MAX_CHUNKED_FILE_BYTES, MAX_FILE_BYTES, ServerPath};
use serde_json::{Value, json};

const PIECE: usize = 4 * 1024 * 1024;

fn server_path(path: &str) -> ServerPath {
    ServerPath(path.as_bytes().to_vec())
}

/// Reads a file as `{ path, base64, size }`. `chunked` says whether the server
/// can read in pieces; without it only files within `MAX_FILE_BYTES` read.
pub(super) async fn read<F, Fut>(path: &str, chunked: bool, mut send: F) -> Result<Value, String>
where
    F: FnMut(FilesAction) -> Fut,
    Fut: Future<Output = Result<FilesReply, String>>,
{
    let unexpected = || "unexpected files response".to_owned();
    let bytes = if chunked {
        let mut bytes = Vec::new();
        let mut size = 0;
        loop {
            let reply = send(FilesAction::ReadRange {
                path: server_path(path),
                offset: bytes.len() as u64,
                length: PIECE as u32,
            })
            .await?;
            let FilesReply::Chunk(chunk) = reply else {
                return Err(unexpected());
            };
            if chunk.size > MAX_CHUNKED_FILE_BYTES {
                return Err("file exceeds the 64 MiB limit".into());
            }
            if chunk.offset != bytes.len() as u64 || (size != 0 && chunk.size != size) {
                return Err("the file changed while it was being read".into());
            }
            size = chunk.size;
            if bytes.is_empty() {
                bytes.reserve_exact(usize::try_from(size).map_err(|_| unexpected())?);
            }
            let done = chunk.bytes.is_empty();
            bytes.extend(chunk.bytes);
            if bytes.len() as u64 >= size {
                break;
            }
            if done {
                return Err("the file changed while it was being read".into());
            }
        }
        bytes
    } else {
        match send(FilesAction::ReadBytes(server_path(path))).await? {
            FilesReply::Bytes(file) => file.bytes,
            _ => return Err(unexpected()),
        }
    };
    Ok(json!({"path": path, "base64": STANDARD.encode(&bytes), "size": bytes.len()}))
}

/// Writes the base64 `data` to `path`, replacing it whole or not at all.
/// `upload` names the temporary file the pieces collect in.
pub(super) async fn write<F, Fut>(
    path: &str,
    data: &str,
    chunked: bool,
    upload: u64,
    mut send: F,
) -> Result<Value, String>
where
    F: FnMut(FilesAction) -> Fut,
    Fut: Future<Output = Result<FilesReply, String>>,
{
    if data.len() / 4 * 3 > MAX_CHUNKED_FILE_BYTES as usize + 3 {
        return Err("file exceeds the 64 MiB limit".into());
    }
    let bytes = STANDARD
        .decode(data.trim())
        .map_err(|_| "base64 must be valid standard base64".to_owned())?;
    if bytes.len() as u64 > MAX_CHUNKED_FILE_BYTES {
        return Err("file exceeds the 64 MiB limit".into());
    }
    let reply = if !chunked {
        if bytes.len() > MAX_FILE_BYTES {
            return Err(
                "this server writes files of at most 5 MiB; update it to write larger files".into(),
            );
        }
        send(FilesAction::WriteBytes {
            path: server_path(path),
            bytes,
        })
        .await?
    } else {
        let mut offset = 0;
        loop {
            let end = (offset + PIECE).min(bytes.len());
            let last = end == bytes.len();
            let result = send(FilesAction::WriteChunk {
                path: server_path(path),
                upload,
                offset: offset as u64,
                bytes: bytes[offset..end].to_vec(),
                commit: last,
                abort: false,
            })
            .await;
            match result {
                Ok(reply) if last => break reply,
                Ok(_) => offset = end,
                Err(error) => {
                    let _ = send(FilesAction::WriteChunk {
                        path: server_path(path),
                        upload,
                        offset: 0,
                        bytes: Vec::new(),
                        commit: false,
                        abort: true,
                    })
                    .await;
                    return Err(error);
                }
            }
        }
    };
    match reply {
        FilesReply::Path(path) => {
            Ok(json!({"path": muxy_app_core::extensions::api::path_text(&path)?}))
        }
        _ => Err("unexpected files response".into()),
    }
}

/// A name for one upload's temporary file that no other upload is using.
pub(super) fn upload_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let clock = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos() as u64);
    clock
        .rotate_left(17)
        .wrapping_add(NEXT.fetch_add(1, Ordering::Relaxed))
        ^ u64::from(std::process::id())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::panic,
        reason = "Tests fail immediately on fixture errors"
    )]
    use super::*;
    use muxy_protocol::{FileBytes, FileChunk};
    use std::cell::RefCell;
    use std::task::{Context, Poll, Waker};

    fn block_on<T>(future: impl Future<Output = T>) -> T {
        let mut future = std::pin::pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("the fake server answers immediately"),
        }
    }

    /// A one-file server holding `content`, with an upload in progress.
    #[derive(Default)]
    struct Fake {
        file: RefCell<Vec<u8>>,
        staged: RefCell<Option<Vec<u8>>>,
        requests: RefCell<Vec<&'static str>>,
    }

    impl Fake {
        fn serve(&self, action: FilesAction) -> Result<FilesReply, String> {
            let path = ServerPath(b"f".to_vec());
            match action {
                FilesAction::ReadBytes(_) => {
                    self.requests.borrow_mut().push("read");
                    Ok(FilesReply::Bytes(FileBytes {
                        path,
                        bytes: self.file.borrow().clone(),
                    }))
                }
                FilesAction::ReadRange { offset, length, .. } => {
                    self.requests.borrow_mut().push("range");
                    let file = self.file.borrow();
                    let start = (offset as usize).min(file.len());
                    let end = (start + length as usize).min(file.len());
                    Ok(FilesReply::Chunk(FileChunk {
                        path,
                        size: file.len() as u64,
                        offset,
                        bytes: file[start..end].to_vec(),
                    }))
                }
                FilesAction::WriteBytes { bytes, .. } => {
                    self.requests.borrow_mut().push("write");
                    *self.file.borrow_mut() = bytes;
                    Ok(FilesReply::Path(path))
                }
                FilesAction::WriteChunk {
                    offset,
                    bytes,
                    commit,
                    abort,
                    ..
                } => {
                    self.requests.borrow_mut().push("chunk");
                    let mut staged = self.staged.borrow_mut();
                    if abort {
                        *staged = None;
                    } else {
                        let current = staged.get_or_insert_with(Vec::new);
                        if current.len() as u64 != offset {
                            return Err("bad offset".into());
                        }
                        current.extend(bytes);
                        if commit {
                            *self.file.borrow_mut() = staged.take().unwrap();
                        }
                    }
                    Ok(FilesReply::Path(path))
                }
                _ => Err("unsupported".into()),
            }
        }
    }

    fn go(
        fake: &Fake,
    ) -> impl FnMut(FilesAction) -> std::future::Ready<Result<FilesReply, String>> + '_ {
        move |action| std::future::ready(fake.serve(action))
    }

    fn binary(len: usize) -> Vec<u8> {
        (0..=255).cycle().take(len).collect()
    }

    #[test]
    fn binary_files_with_nul_bytes_round_trip_in_pieces() {
        let data = binary(PIECE * 2 + 17);
        let fake = Fake::default();
        let encoded = STANDARD.encode(&data);
        let written = block_on(write("f", &encoded, true, 1, go(&fake))).unwrap();
        assert_eq!(written["path"], "f");
        assert_eq!(*fake.file.borrow(), data);
        assert_eq!(
            fake.requests
                .borrow()
                .iter()
                .filter(|r| **r == "chunk")
                .count(),
            3
        );
        let read = block_on(read("f", true, go(&fake))).unwrap();
        assert_eq!(read["size"], data.len());
        assert_eq!(
            STANDARD.decode(read["base64"].as_str().unwrap()).unwrap(),
            data
        );
    }

    #[test]
    fn empty_files_commit_and_read_back() {
        let fake = Fake::default();
        block_on(write("f", "", true, 1, go(&fake))).unwrap();
        assert!(fake.file.borrow().is_empty());
        let read = block_on(read("f", true, go(&fake))).unwrap();
        assert_eq!(read["size"], 0);
    }

    #[test]
    fn servers_without_chunks_handle_only_small_files() {
        let fake = Fake::default();
        let small = STANDARD.encode(binary(1000));
        block_on(write("f", &small, false, 1, go(&fake))).unwrap();
        assert_eq!(fake.file.borrow().len(), 1000);
        assert_eq!(fake.requests.borrow().as_slice(), ["write"]);
        let read = block_on(read("f", false, go(&fake))).unwrap();
        assert_eq!(read["size"], 1000);
        let large = STANDARD.encode(binary(MAX_FILE_BYTES + 1));
        let error = block_on(write("f", &large, false, 1, go(&fake))).unwrap_err();
        assert!(error.contains("5 MiB"), "{error}");
        assert_eq!(fake.file.borrow().len(), 1000);
    }

    #[test]
    fn files_over_the_limit_and_bad_base64_are_refused_before_any_request() {
        let fake = Fake::default();
        let over = STANDARD.encode(vec![0_u8; MAX_CHUNKED_FILE_BYTES as usize + 1]);
        assert!(block_on(write("f", &over, true, 1, go(&fake))).is_err());
        assert!(block_on(write("f", "***", true, 1, go(&fake))).is_err());
        assert!(fake.requests.borrow().is_empty());
        *fake.file.borrow_mut() = vec![0; 8];
        let error = block_on(read("f", true, |action| {
            std::future::ready(fake.serve(action).map(|reply| match reply {
                FilesReply::Chunk(mut chunk) => {
                    chunk.size = MAX_CHUNKED_FILE_BYTES + 1;
                    FilesReply::Chunk(chunk)
                }
                other => other,
            }))
        }))
        .unwrap_err();
        assert!(error.contains("64 MiB"), "{error}");
    }

    #[test]
    fn a_failed_piece_aborts_the_upload_and_keeps_the_old_file() {
        let fake = Fake::default();
        *fake.file.borrow_mut() = b"old".to_vec();
        let data = STANDARD.encode(binary(PIECE + 1));
        let mut calls = 0;
        let error = block_on(write("f", &data, true, 1, |action| {
            calls += 1;
            let failing = calls == 2;
            let reply = if failing {
                Err("disk full".into())
            } else {
                fake.serve(action)
            };
            std::future::ready(reply)
        }))
        .unwrap_err();
        assert_eq!(error, "disk full");
        assert_eq!(*fake.file.borrow(), b"old");
        assert!(fake.staged.borrow().is_none());
    }

    #[test]
    fn upload_ids_differ() {
        assert_ne!(upload_id(), upload_id());
    }
}
