use std::{
    fs::File,
    io::{self, Read, Write},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

use crate::{
    model::{ChatMessageRole, SessionIdentity},
    storage::{set_file_mode, validate_private_regular_file, SecureDirectory},
};

pub const TRANSCRIPT_RECORD_VERSION: u32 = 1;
pub const MAX_TRANSCRIPT_RECORD_BYTES: usize = 1024 * 1024;
pub const MAX_TRANSCRIPT_FILE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptRecord {
    pub version: u32,
    pub identity: SessionIdentity,
    pub sequence: u64,
    pub role: ChatMessageRole,
    pub text: String,
}

pub struct TranscriptJournal {
    path: PathBuf,
    identity: SessionIdentity,
    file: File,
    records: Vec<TranscriptRecord>,
    next_sequence: u64,
    bytes: usize,
}

impl TranscriptJournal {
    /// Opens an append-only journal for authoritative structured messages.
    /// Arbitrary PTY output must not be passed to this API because byte chunks
    /// do not establish message roles or boundaries.
    pub fn open(path: PathBuf, identity: SessionIdentity) -> io::Result<Self> {
        let directory = SecureDirectory::open_parent(&path, true, true)?;
        let name = path.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("transcript path must name a file: {}", path.display()),
            )
        })?;
        let mut file = directory.open_file(
            name,
            libc::O_RDWR | libc::O_APPEND | libc::O_CREAT | libc::O_NONBLOCK,
            0o600,
        )?;
        validate_private_regular_file(&file, &format!("transcript file {}", path.display()))?;
        set_file_mode(&file, 0o600)?;

        let (records, bytes) = read_and_recover(&mut file, identity)?;
        let next_sequence = records.last().map_or(Ok(1), |record| {
            record.sequence.checked_add(1).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "transcript sequence is exhausted",
                )
            })
        })?;

        Ok(Self {
            path,
            identity,
            file,
            records,
            next_sequence,
            bytes,
        })
    }

    pub fn records(&self) -> &[TranscriptRecord] {
        &self.records
    }

    pub fn append(&mut self, role: ChatMessageRole, text: String) -> io::Result<u64> {
        let sequence = self.next_sequence;
        let record = TranscriptRecord {
            version: TRANSCRIPT_RECORD_VERSION,
            identity: self.identity,
            sequence,
            role,
            text,
        };
        let mut encoded = serde_json::to_vec(&record).map_err(invalid_data)?;
        encoded.push(b'\n');
        if encoded.len() > MAX_TRANSCRIPT_RECORD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "transcript record exceeds {MAX_TRANSCRIPT_RECORD_BYTES} bytes for {}",
                    self.path.display()
                ),
            ));
        }
        let new_size = self.bytes.checked_add(encoded.len()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "transcript size overflow")
        })?;
        if new_size > MAX_TRANSCRIPT_FILE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                format!(
                    "transcript exceeds {MAX_TRANSCRIPT_FILE_BYTES} bytes for {}",
                    self.path.display()
                ),
            ));
        }

        self.file.write_all(&encoded)?;
        self.file.sync_data()?;
        self.records.push(record);
        self.next_sequence = sequence.checked_add(1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "transcript sequence is exhausted",
            )
        })?;
        self.bytes = new_size;
        Ok(sequence)
    }
}

fn read_and_recover(
    file: &mut File,
    identity: SessionIdentity,
) -> io::Result<(Vec<TranscriptRecord>, usize)> {
    let mut bytes = Vec::new();
    file.take((MAX_TRANSCRIPT_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_TRANSCRIPT_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("transcript exceeds {MAX_TRANSCRIPT_FILE_BYTES} bytes"),
        ));
    }

    let last_complete = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |position| position + 1);
    let complete = &bytes[..last_complete];
    let mut records = Vec::new();
    let mut expected_sequence = 1_u64;
    for encoded in complete
        .strip_suffix(b"\n")
        .into_iter()
        .flat_map(|records| records.split(|byte| *byte == b'\n'))
    {
        if encoded.is_empty() || encoded.len() + 1 > MAX_TRANSCRIPT_RECORD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "transcript contains an empty or oversized complete record",
            ));
        }
        let record: TranscriptRecord = serde_json::from_slice(encoded).map_err(invalid_data)?;
        if record.version != TRANSCRIPT_RECORD_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported transcript record version {}", record.version),
            ));
        }
        if record.identity != identity {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "transcript record belongs to another session identity",
            ));
        }
        if record.sequence != expected_sequence {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "transcript sequence gap: expected {expected_sequence}, found {}",
                    record.sequence
                ),
            ));
        }
        expected_sequence = expected_sequence.checked_add(1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "transcript sequence is exhausted",
            )
        })?;
        records.push(record);
    }

    if last_complete != bytes.len() {
        file.set_len(last_complete as u64)?;
        file.sync_data()?;
    }
    Ok((records, last_complete))
}

fn invalid_data(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{self, OpenOptions},
        os::unix::fs::PermissionsExt,
        sync::atomic::{AtomicU64, Ordering},
    };

    use crate::model::{ProjectState, PtyKey};

    use super::*;

    static NEXT_TEST_PATH: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn truncated_final_record_is_removed_without_losing_prior_records() {
        let path = unique_test_path();
        let identity = identity();
        let mut journal = TranscriptJournal::open(path.clone(), identity).unwrap();
        journal
            .append(ChatMessageRole::User, "hello".to_string())
            .unwrap();
        journal
            .append(ChatMessageRole::Assistant, "hi".to_string())
            .unwrap();
        let complete_len = fs::metadata(&path).unwrap().len();
        drop(journal);

        let mut raw = OpenOptions::new().append(true).open(&path).unwrap();
        raw.write_all(br#"{"version":1,"identity":"#).unwrap();
        raw.sync_data().unwrap();
        drop(raw);

        let journal = TranscriptJournal::open(path.clone(), identity).unwrap();

        assert_eq!(journal.records().len(), 2);
        assert_eq!(journal.records()[0].text, "hello");
        assert_eq!(journal.records()[1].text, "hi");
        assert_eq!(fs::metadata(path).unwrap().len(), complete_len);
    }

    #[test]
    fn malformed_complete_record_is_not_silently_discarded() {
        let path = unique_test_path();
        let identity = identity();
        let journal = TranscriptJournal::open(path.clone(), identity).unwrap();
        drop(journal);
        fs::write(&path, b"not-json\n").unwrap();

        let error = TranscriptJournal::open(path, identity)
            .err()
            .expect("complete malformed record must fail");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn wrong_identity_record_is_rejected_without_discarding_it() {
        let path = unique_test_path();
        let identity = identity();
        let other = {
            let state = ProjectState::try_default().unwrap();
            let terminal = state.workspaces[1].terminals[0].id;
            state.session_identity(PtyKey::Terminal(terminal)).unwrap()
        };
        let encoded = serde_json::to_vec(&TranscriptRecord {
            version: TRANSCRIPT_RECORD_VERSION,
            identity: other,
            sequence: 1,
            role: ChatMessageRole::User,
            text: "wrong chat".to_string(),
        })
        .unwrap();
        let mut bytes = encoded;
        bytes.push(b'\n');
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, &bytes).unwrap();

        let error = TranscriptJournal::open(path.clone(), identity)
            .err()
            .expect("wrong identity must fail");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn oversized_record_is_rejected_before_append() {
        let path = unique_test_path();
        let identity = identity();
        let mut journal = TranscriptJournal::open(path.clone(), identity).unwrap();

        let error = journal
            .append(
                ChatMessageRole::Assistant,
                "x".repeat(MAX_TRANSCRIPT_RECORD_BYTES),
            )
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::metadata(path).unwrap().len(), 0);
    }

    #[test]
    fn append_is_ordered_identity_scoped_and_owner_private() {
        let path = unique_test_path();
        let identity = identity();
        let mut journal = TranscriptJournal::open(path.clone(), identity).unwrap();

        assert_eq!(
            journal
                .append(ChatMessageRole::User, "question".to_string())
                .unwrap(),
            1
        );
        assert_eq!(
            journal
                .append(ChatMessageRole::Assistant, "answer".to_string())
                .unwrap(),
            2
        );
        assert_eq!(journal.records()[1].identity, identity);
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    fn identity() -> SessionIdentity {
        let state = ProjectState::try_default().unwrap();
        let terminal = state.workspaces[0].terminals[0].id;
        state.session_identity(PtyKey::Terminal(terminal)).unwrap()
    }

    fn unique_test_path() -> PathBuf {
        let sequence = NEXT_TEST_PATH.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!(
                "mult-transcript-test-{}-{sequence}",
                std::process::id()
            ))
            .join("chat.jsonl")
    }
}
