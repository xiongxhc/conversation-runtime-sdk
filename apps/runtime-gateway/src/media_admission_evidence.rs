//! Compile-time-only, content-free private media decision journal.
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use ring::rand::{SecureRandom, SystemRandom};
use serde::Serialize;

pub(crate) const ENV_PATH: &str = "RUNTIME_MEDIA_ADMISSION_EVIDENCE_PATH";
const SCHEMA: &str = "runtime-media-admission-evidence/v1";
const MAX_RECORDS: u64 = 4096;
const MAX_LINE_BYTES: usize = 2048;
const QUEUE_CAPACITY: usize = 64;

#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Transport {
    Tcp {
        peer: SocketAddr,
        local: SocketAddr,
    },
    #[cfg(test)]
    Component,
}

#[derive(Clone, Serialize)]
pub(crate) struct Context {
    pub attempt_id: u64,
    pub transport: Transport,
}

#[derive(Serialize)]
pub(crate) struct Predicates {
    pub session_present: bool,
    pub credential_matches: bool,
    pub media_version_matches: bool,
    pub bridge_binding_matches: bool,
    pub credential_expired: bool,
    pub reconnect_expired: bool,
    pub active: bool,
    pub capture_finished: bool,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Record {
    Header,
    Admission {
        session_id: String,
        reservation_epoch: String,
        connection_id: Option<String>,
        accepted: bool,
        context: Context,
        predicates: Predicates,
    },
    MissingSession {
        session_id: String,
        session_present: bool,
        context: Context,
    },
    FenceRelease {
        session_id: String,
        reservation_epoch: String,
        connection_id: String,
    },
    Completion {
        record_count: u64,
        loss: bool,
        writer_error: bool,
    },
}

#[derive(Serialize)]
struct Envelope {
    schema: &'static str,
    instance: String,
    sequence: u64,
    #[serde(flatten)]
    record: Record,
}

struct Queue {
    sender: Option<mpsc::SyncSender<Envelope>>,
    next_sequence: u64,
}

pub(crate) struct Journal {
    instance: String,
    queue: Mutex<Queue>,
    invalid: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    complete: AtomicBool,
    next_attempt: AtomicU64,
    worker: tokio::sync::Mutex<Option<std::thread::JoinHandle<io::Result<()>>>>,
}

fn failure() -> io::Error {
    io::Error::other("media evidence unavailable")
}

impl Journal {
    pub fn from_env() -> io::Result<Option<Arc<Self>>> {
        std::env::var_os(ENV_PATH)
            .map(|path| Self::open(Path::new(&path)).map(Arc::new))
            .transpose()
    }

    pub fn open(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
            if !path.is_absolute() || path.as_os_str().len() > 1024 {
                return Err(failure());
            }
            let parent = path.parent().ok_or_else(failure)?;
            // Reject symlinked ancestors and a non-private parent before create.
            if std::fs::canonicalize(parent)? != parent {
                return Err(failure());
            }
            let before = std::fs::symlink_metadata(parent)?;
            if !before.is_dir() || before.mode() & 0o077 != 0 {
                return Err(failure());
            }
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)?;
            let own = file.metadata()?;
            let after = std::fs::symlink_metadata(parent)?;
            // The kernel owns a newly created file to this process. Comparing
            // that owner avoids unsafe UID calls or trusting environment text.
            if before.dev() != after.dev()
                || before.ino() != after.ino()
                || before.uid() != own.uid()
                || after.mode() & 0o077 != 0
                || own.mode() & 0o777 != 0o600
                || !own.is_file()
                || own.nlink() != 1
            {
                return Err(failure());
            }
            Self::start(file)
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Err(failure())
        }
    }

    fn start(file: File) -> io::Result<Self> {
        Self::start_writer(
            file,
            #[cfg(test)]
            None,
        )
    }

    fn start_writer(
        mut file: File,
        #[cfg(test)] gate: Option<mpsc::Receiver<()>>,
    ) -> io::Result<Self> {
        let mut nonce = [0u8; 16];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| failure())?;
        let instance: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
        let (sender, receiver) = mpsc::sync_channel::<Envelope>(QUEUE_CAPACITY);
        let invalid = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let writer_invalid = invalid.clone();
        let writer_finished = finished.clone();
        let writer_instance = instance.clone();
        let worker = std::thread::Builder::new()
            .name("media-evidence-writer".into())
            .spawn(move || {
                #[cfg(test)]
                if let Some(gate) = gate {
                    gate.recv().map_err(|_| failure())?;
                }
                let write = |file: &mut File, envelope: &Envelope| -> io::Result<()> {
                    let mut line = serde_json::to_vec(envelope).map_err(|_| failure())?;
                    line.push(b'\n');
                    if line.len() > MAX_LINE_BYTES {
                        return Err(failure());
                    }
                    file.write_all(&line)
                };
                let result = (|| {
                    write(
                        &mut file,
                        &Envelope {
                            schema: SCHEMA,
                            instance: writer_instance.clone(),
                            sequence: 0,
                            record: Record::Header,
                        },
                    )?;
                    let mut sequence = 1;
                    for envelope in receiver {
                        if envelope.sequence != sequence {
                            return Err(failure());
                        }
                        write(&mut file, &envelope)?;
                        // Tail consumers need actual release visibility, not a
                        // userspace buffer; acceptance still requires completion.
                        file.flush()?;
                        sequence += 1;
                    }
                    if writer_invalid.load(Ordering::Acquire)
                        || !writer_finished.load(Ordering::Acquire)
                    {
                        return Err(failure());
                    }
                    file.sync_all()?;
                    write(
                        &mut file,
                        &Envelope {
                            schema: SCHEMA,
                            instance: writer_instance,
                            sequence,
                            record: Record::Completion {
                                record_count: sequence + 1,
                                loss: false,
                                writer_error: false,
                            },
                        },
                    )?;
                    file.sync_all()
                })();
                if result.is_err() {
                    writer_invalid.store(true, Ordering::Release);
                }
                result
            })?;
        Ok(Self {
            instance,
            queue: Mutex::new(Queue {
                sender: Some(sender),
                next_sequence: 1,
            }),
            invalid,
            finished,
            complete: AtomicBool::new(false),
            next_attempt: AtomicU64::new(1),
            worker: tokio::sync::Mutex::new(Some(worker)),
        })
    }

    pub fn context(&self, transport: Transport) -> Context {
        let attempt_id = self.next_attempt.fetch_add(1, Ordering::Relaxed);
        if attempt_id > MAX_RECORDS {
            self.invalidate();
        }
        Context {
            attempt_id,
            transport,
        }
    }

    pub fn invalidate(&self) {
        self.invalid.store(true, Ordering::Release);
    }

    pub fn record(&self, record: Record) {
        // Never wait while the media registry mutex is held. Sequence assignment
        // and enqueue are one nonblocking critical section; contention is loss.
        if self.invalid.load(Ordering::Acquire) {
            return;
        }
        let Ok(mut queue) = self.queue.try_lock() else {
            self.invalidate();
            return;
        };
        if queue.next_sequence >= MAX_RECORDS - 1 {
            self.invalidate();
            return;
        }
        let envelope = Envelope {
            schema: SCHEMA,
            instance: self.instance.clone(),
            sequence: queue.next_sequence,
            record,
        };
        let Some(sender) = &queue.sender else {
            self.invalidate();
            return;
        };
        if sender.try_send(envelope).is_err() {
            self.invalidate();
            return;
        }
        queue.next_sequence += 1;
    }

    pub async fn finish(&self) {
        let mut worker = self.worker.lock().await;
        if let Some(join) = worker.take() {
            let sender = self
                .queue
                .lock()
                .unwrap_or_else(|poisoned| {
                    self.invalidate();
                    poisoned.into_inner()
                })
                .sender
                .take();
            self.finished.store(true, Ordering::Release);
            drop(sender);
            let result = tokio::task::spawn_blocking(move || join.join()).await;
            let ok = matches!(result, Ok(Ok(Ok(())))) && !self.invalid.load(Ordering::Acquire);
            self.complete.store(ok, Ordering::Release);
        }
    }

    pub fn complete(&self) -> bool {
        self.complete.load(Ordering::Acquire) && !self.invalid.load(Ordering::Acquire)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn private_path(temp: &tempfile::TempDir) -> std::path::PathBuf {
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        temp.path().canonicalize().unwrap().join("journal.jsonl")
    }
    fn missing(journal: &Journal) -> Record {
        Record::MissingSession {
            session_id: "41".into(),
            session_present: false,
            context: journal.context(Transport::Component),
        }
    }
    fn rows(path: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn media_evidence_private_file_exclusive_identity_and_same_instance_order() {
        let temp = tempfile::tempdir().unwrap();
        let path = private_path(&temp);
        let journal = Journal::open(&path).unwrap();
        let identity = std::fs::symlink_metadata(&path).unwrap();
        assert_eq!(identity.mode() & 0o777, 0o600);
        assert_eq!(identity.nlink(), 1);
        assert!(Journal::open(&path).is_err());
        for _ in 0..8 {
            journal.record(missing(&journal));
        }
        journal.finish().await;
        assert!(journal.complete());
        let after = std::fs::symlink_metadata(&path).unwrap();
        assert_eq!((identity.dev(), identity.ino()), (after.dev(), after.ino()));
        let records = rows(&path);
        assert_eq!(records.len(), 10);
        for (sequence, row) in records.iter().enumerate() {
            assert_eq!(row["sequence"], sequence);
            assert_eq!(row["instance"], records[0]["instance"]);
            assert_eq!(row["schema"], SCHEMA);
        }
        assert_eq!(records[9]["type"], "completion");
        assert_eq!(records[9]["record_count"], 10);
        assert_eq!(records[9]["loss"], false);
        assert_eq!(records[9]["writer_error"], false);
        assert!(std::fs::read(&path)
            .unwrap()
            .split(|byte| *byte == b'\n')
            .all(|line| line.len() < MAX_LINE_BYTES));
    }

    #[test]
    fn media_evidence_nonprivate_parent_symlink_target_and_symlink_ancestor_refuse() {
        let temp = tempfile::tempdir().unwrap();
        let path = private_path(&temp);
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Journal::open(&path).is_err());
        assert!(!path.exists());
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink("missing", &path).unwrap();
        assert!(Journal::open(&path).is_err());
        let parent_link = path.with_file_name("parent-link");
        std::os::unix::fs::symlink(path.parent().unwrap(), &parent_link).unwrap();
        assert!(Journal::open(&parent_link.join("new.jsonl")).is_err());
    }

    #[tokio::test]
    async fn media_evidence_enqueue_contention_invalidates_without_blocking_or_valid_completion() {
        let temp = tempfile::tempdir().unwrap();
        let path = private_path(&temp);
        let journal = Journal::open(&path).unwrap();
        {
            let held = journal.queue.lock().unwrap();
            journal.record(missing(&journal));
            assert!(journal.invalid.load(Ordering::Acquire));
            assert_eq!(held.next_sequence, 1);
        }
        journal.finish().await;
        assert!(!journal.complete());
        assert!(rows(&path).iter().all(|row| row["type"] != "completion"));
    }

    #[tokio::test]
    async fn media_evidence_queue_overflow_has_no_sequence_gap_or_valid_completion() {
        let temp = tempfile::tempdir().unwrap();
        let path = private_path(&temp);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let (release, gate) = mpsc::channel();
        let journal = Journal::start_writer(file, Some(gate)).unwrap();
        for _ in 0..QUEUE_CAPACITY {
            journal.record(missing(&journal));
        }
        assert!(!journal.invalid.load(Ordering::Acquire));
        journal.record(missing(&journal));
        assert!(journal.invalid.load(Ordering::Acquire));
        assert_eq!(
            journal.queue.lock().unwrap().next_sequence,
            QUEUE_CAPACITY as u64 + 1
        );
        release.send(()).unwrap();
        journal.finish().await;
        assert!(!journal.complete());
        let records = rows(&path);
        assert_eq!(records.len(), QUEUE_CAPACITY + 1);
        for (sequence, row) in records.iter().enumerate() {
            assert_eq!(row["sequence"], sequence);
        }
        assert!(records.iter().all(|row| row["type"] != "completion"));
    }

    #[tokio::test]
    async fn media_evidence_actual_writer_error_leaves_incomplete_private_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let path = private_path(&temp);
        std::fs::write(&path, b"").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        // Unit negative fixture: an actual read-only File makes the real writer
        // fail. It does not simulate successful gateway evidence.
        let journal = Journal::start(File::open(&path).unwrap()).unwrap();
        journal.record(missing(&journal));
        journal.finish().await;
        assert!(!journal.complete());
        assert!(journal.invalid.load(Ordering::Acquire));
        assert!(std::fs::read(&path).unwrap().is_empty());
    }

    #[tokio::test]
    async fn media_evidence_drop_without_finish_and_truncated_journal_are_never_complete() {
        let temp = tempfile::tempdir().unwrap();
        let path = private_path(&temp);
        let journal = Journal::open(&path).unwrap();
        journal.record(missing(&journal));
        assert!(!journal.complete());
        let worker = journal.worker.lock().await.take().unwrap();
        drop(journal);
        assert!(tokio::task::spawn_blocking(move || worker.join())
            .await
            .unwrap()
            .unwrap()
            .is_err());
        let records = rows(&path);
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|row| row["type"] != "completion"));
        // A consumer must reject the unfinished final JSON line, independently
        // of any preceding complete-looking records.
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() - 4]).unwrap();
        let truncated = std::fs::read_to_string(&path).unwrap();
        assert!(!truncated.ends_with('\n'));
        assert!(
            serde_json::from_str::<serde_json::Value>(truncated.lines().last().unwrap()).is_err()
        );
    }
}
