use muxy_protocol::{
    FileBytes, FileChanges, FileContent, FilesAction, FilesReply, FilesRequest, MAX_FILE_BYTES,
    MAX_FILE_CHANGES, ProjectId, ServerPath,
};

fn p(value: &str) -> ServerPath {
    ServerPath(value.as_bytes().to_vec())
}

#[test]
fn files_validation_bounds_content_names_selections_and_events() {
    for action in [
        FilesAction::Read(p("/absolute")),
        FilesAction::Stat(p("nul\0")),
        FilesAction::Write {
            path: p("file"),
            content: "a".repeat(MAX_FILE_BYTES + 1),
        },
        FilesAction::Rename {
            path: p("file"),
            name: p("../other"),
        },
        FilesAction::Rename {
            path: p("file"),
            name: p(".."),
        },
        FilesAction::Delete(vec![p("file"); 4097]),
        FilesAction::Read(p(&"a".repeat(4097))),
        FilesAction::ReadBytes(p("/absolute")),
        FilesAction::WriteBytes {
            path: p("file"),
            bytes: vec![0; MAX_FILE_BYTES + 1],
        },
        FilesAction::WriteBytes {
            path: p("/absolute"),
            bytes: Vec::new(),
        },
    ] {
        assert!(
            FilesRequest {
                project: ProjectId::new(),
                action
            }
            .validate()
            .is_err()
        );
    }
    assert!(
        FilesReply::Content(FileContent {
            path: p("file"),
            content: "text".into(),
            size: 1
        })
        .validate()
        .is_err()
    );
    for bytes in [
        FileBytes {
            path: p("file"),
            bytes: vec![0; MAX_FILE_BYTES + 1],
        },
        FileBytes {
            path: p("/absolute"),
            bytes: Vec::new(),
        },
    ] {
        assert!(FilesReply::Bytes(bytes).validate().is_err());
    }
    assert!(
        FileChanges {
            paths: vec![p("file"); MAX_FILE_CHANGES + 1],
            rescan: false
        }
        .validate()
        .is_err()
    );
}

#[test]
fn change_merging_deduplicates_and_converts_overflow_to_rescan() {
    let mut changes = FileChanges::default();
    for _ in 0..5 {
        changes.merge(&FileChanges {
            paths: vec![p("file")],
            rescan: false,
        });
    }
    assert_eq!(changes.paths, vec![p("file")]);
    for index in 0..MAX_FILE_CHANGES {
        changes.merge(&FileChanges {
            paths: vec![p(&index.to_string())],
            rescan: false,
        });
    }
    assert!(changes.rescan);
    assert!(changes.paths.is_empty());
    changes.merge(&FileChanges {
        paths: vec![p("later")],
        rescan: false,
    });
    assert!(changes.paths.is_empty());
}

#[test]
fn uploads_come_in_bounded_chunks_and_reply_with_an_absolute_path() {
    use muxy_protocol::{
        ErrorCode, MAX_UPLOAD_BYTES, MAX_UPLOAD_CHUNK, MAX_UPLOAD_NAME, OperationId, ServerPath,
        SessionId, UploadChunk, validate_uploaded,
    };
    let chunk = |offset: u64, length: usize, name: &str| UploadChunk {
        session: SessionId::new(1).expect("session"),
        upload: OperationId::new(),
        name: name.into(),
        offset,
        bytes: vec![0; length],
        last: false,
    };
    assert_eq!(chunk(0, MAX_UPLOAD_CHUNK, "shot.png").validate(), Ok(()));
    let last = MAX_UPLOAD_BYTES - MAX_UPLOAD_CHUNK as u64;
    assert_eq!(chunk(last, MAX_UPLOAD_CHUNK, "a").validate(), Ok(()));
    for bad in [
        chunk(0, MAX_UPLOAD_CHUNK + 1, "a"),
        chunk(last + 1, MAX_UPLOAD_CHUNK, "a"),
        chunk(u64::MAX, 1, "a"),
        chunk(0, 1, &"a".repeat(MAX_UPLOAD_NAME + 1)),
    ] {
        assert_eq!(bad.validate(), Err(ErrorCode::BadRequest));
    }
    let path = |bytes: &[u8]| ServerPath(bytes.to_vec());
    assert_eq!(validate_uploaded(None), Ok(()));
    assert_eq!(validate_uploaded(Some(&path(b"/srv/u/shot.png"))), Ok(()));
    for bad in [&b"relative.png"[..], b"/a\0b", b""] {
        assert_eq!(validate_uploaded(Some(&path(bad))), Err(ErrorCode::BadPath));
    }
}

#[test]
fn chunked_file_requests_are_bounded() {
    use muxy_protocol::MAX_CHUNKED_FILE_BYTES;
    let request = |action| FilesRequest {
        project: ProjectId::from_u128(1),
        action,
    };
    let chunk = |offset: u64, len: usize, commit: bool, abort: bool| {
        request(FilesAction::WriteChunk {
            path: p("file"),
            upload: 1,
            offset,
            bytes: vec![0; len],
            commit,
            abort,
        })
    };
    assert!(chunk(0, MAX_FILE_BYTES, true, false).validate().is_ok());
    assert!(
        chunk(0, MAX_FILE_BYTES + 1, false, false)
            .validate()
            .is_err()
    );
    assert!(
        chunk(MAX_CHUNKED_FILE_BYTES, 1, false, false)
            .validate()
            .is_err()
    );
    assert!(chunk(0, 0, true, true).validate().is_err());
    let range = |length| {
        request(FilesAction::ReadRange {
            path: p("file"),
            offset: 0,
            length,
        })
    };
    let limit = u32::try_from(MAX_FILE_BYTES).expect("fits");
    assert!(range(limit).validate().is_ok());
    assert!(range(limit + 1).validate().is_err());
}
