use super::*;
use crate::reconcile_finalized_recording;
use conversation_model_adapters::{AdapterFuture, DiarizationResult, MeetingAudioWindows};
use conversation_protocol::{GenerationId, TurnId, UtteranceId};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
static NEXT_KEY: AtomicUsize = AtomicUsize::new(0);

// This test-only allocation boundary makes the sparse-file RED safe: entering the
// snapshot path is observable and refused before allocating or copying any bytes.
static FORBIDDEN_SNAPSHOTS: std::sync::Mutex<Vec<(PathBuf, Arc<AtomicUsize>)>> =
    std::sync::Mutex::new(Vec::new());
pub(super) fn observe_snapshot_allocation(path: &Path) -> Result<(), RuntimeError> {
    if let Some((_, calls)) = FORBIDDEN_SNAPSHOTS
        .lock()
        .unwrap()
        .iter()
        .find(|(candidate, _)| candidate == path)
    {
        calls.fetch_add(1, Ordering::SeqCst);
        return Err(recovery_error());
    }
    Ok(())
}
#[cfg(unix)]
struct ForbidSnapshot(PathBuf, Arc<AtomicUsize>);
#[cfg(unix)]
impl ForbidSnapshot {
    fn new(path: &Path) -> Self {
        let calls = Arc::new(AtomicUsize::new(0));
        FORBIDDEN_SNAPSHOTS
            .lock()
            .unwrap()
            .push((path.to_owned(), calls.clone()));
        Self(path.to_owned(), calls)
    }
}
#[cfg(unix)]
impl Drop for ForbidSnapshot {
    fn drop(&mut self) {
        FORBIDDEN_SNAPSHOTS
            .lock()
            .unwrap()
            .retain(|(path, _)| *path != self.0);
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    settings: RecordingSettings,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let variable = format!(
            "RUNTIME_CAPABILITY_TEST_{}",
            NEXT_KEY.fetch_add(1, Ordering::Relaxed)
        );
        std::env::set_var(&variable, KEY);
        let settings = RecordingSettings::new(directory.path(), variable).unwrap();
        Self {
            directory,
            settings,
        }
    }
    fn recorder(&self) -> SessionRecorder {
        SessionRecorder::start(&self.settings, SessionId::new(41)).unwrap()
    }
    fn assert_no_snapshot(&self) {
        for entry in std::fs::read_dir(self.directory.path()).unwrap() {
            assert!(!entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".recovery-"));
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::env::remove_var(self.settings.key_env());
    }
}

fn frame(sequence: u64) -> AudioFrame {
    AudioFrame::new(
        TurnId::new(1),
        GenerationId::new(1),
        UtteranceId::new(1),
        sequence,
        PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
        vec![0x35; 32_000],
    )
    .unwrap()
}

#[derive(Default)]
struct Diarizer {
    calls: AtomicUsize,
    bytes: AtomicUsize,
    mutate: Option<PathBuf>,
    cancel: bool,
}
impl conversation_model_adapters::TranscriptionDiarizer for Diarizer {
    fn diarize<'a>(
        &'a self,
        mut windows: MeetingAudioWindows,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, DiarizationResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(path) = &self.mutate {
            // In-place truncation preserves the original inode, after preflight but before
            // consuming its second pass. Descriptor retention alone would fail this test.
            std::fs::write(path, b"mutated original inode").unwrap();
        }
        Box::pin(async move {
            while let Some(window) = windows.recv().await {
                self.bytes
                    .fetch_add(window?.bytes().len(), Ordering::SeqCst);
                if self.cancel {
                    cancellation.cancel();
                }
            }
            DiarizationResult::new(Vec::new(), Vec::new())
        })
    }
}

#[tokio::test]
async fn durable_capability_reopens_with_original_provenance_and_is_deterministic() {
    let fixture = Fixture::new();
    let mut recorder = fixture.recorder();
    recorder.record_frame(&frame(0)).unwrap();
    let (summary, capability) = recorder.finalize_with_capability().unwrap();
    assert_eq!(
        recorder.finalize_with_capability().unwrap(),
        (summary.clone(), capability.clone())
    );
    assert_eq!(capability.source_session_id, SessionId::new(41));
    assert_eq!(capability.sha256, summary.sha256);
    assert_eq!(
        format!("{capability:?}"),
        "FinalizedRecordingCapability([REDACTED])"
    );
    drop(recorder);
    let diarizer = Arc::new(Diarizer::default());
    reconcile_finalized_recording(
        &fixture.settings,
        &capability,
        diarizer.clone(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(diarizer.calls.load(Ordering::SeqCst), 1);
    assert_eq!(diarizer.bytes.load(Ordering::SeqCst), 32_000);
    fixture.assert_no_snapshot();
}

#[tokio::test]
async fn every_authorization_field_wrong_key_and_ciphertext_mutation_fail_before_provider_call() {
    let fixture = Fixture::new();
    let mut recorder = fixture.recorder();
    recorder.record_frame(&frame(0)).unwrap();
    let (summary, capability) = recorder.finalize_with_capability().unwrap();
    let diarizer = Arc::new(Diarizer::default());
    for field in 0..7 {
        let mut forged = capability.clone();
        match field {
            0 => forged.source_session_id = SessionId::new(42),
            1 => forged.recording_handle = "0".repeat(32),
            2 => forged.sha256 = "0".repeat(64),
            3 => forged.key_id = "0".repeat(16),
            4 => forged.authorization = "0".repeat(64),
            5 => forged.recording_handle = "../private".into(),
            _ => forged.authorization = "SENSITIVE-CAPABILITY-MARKER".into(),
        }
        let error = reconcile_finalized_recording(
            &fixture.settings,
            &forged,
            diarizer.clone(),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(!format!("{error:?}").contains("SENSITIVE"));
    }
    std::env::set_var(fixture.settings.key_env(), "ab".repeat(32));
    assert!(reconcile_finalized_recording(
        &fixture.settings,
        &capability,
        diarizer.clone(),
        CancellationToken::new()
    )
    .await
    .is_err());
    std::env::set_var(fixture.settings.key_env(), KEY);
    let original = std::fs::read(&summary.encrypted_path).unwrap();
    for offset in [12, 28, 60, original.len() - 1] {
        let mut corrupt = original.clone();
        corrupt[offset] ^= 1;
        std::fs::write(&summary.encrypted_path, corrupt).unwrap();
        assert!(reconcile_finalized_recording(
            &fixture.settings,
            &capability,
            diarizer.clone(),
            CancellationToken::new()
        )
        .await
        .is_err());
    }
    assert_eq!(diarizer.calls.load(Ordering::SeqCst), 0);
    fixture.assert_no_snapshot();
}

#[tokio::test]
async fn fully_authenticated_preflight_rejects_signed_bad_chunks_and_every_torn_or_extra_tail() {
    let fixture = Fixture::new();
    let diarizer = Arc::new(Diarizer::default());
    for malformed in 0..9 {
        let mut recorder = fixture.recorder();
        recorder.record_frame(&frame(0)).unwrap();
        if malformed == 0 {
            recorder.append(ended_record(0)).unwrap();
        }
        if malformed == 1 {
            recorder.append(gap_ended_record(0)).unwrap();
        }
        if malformed == 2 {
            recorder.append(vec![CHUNK_FRAME; 24]).unwrap();
        }
        if malformed == 3 {
            let mut gap = vec![CHUNK_GAP_STARTED];
            gap.extend_from_slice(&0_u64.to_be_bytes());
            gap.extend_from_slice(&[0, 99]);
            recorder.append(gap).unwrap();
        }
        let (summary, mut capability) = recorder.finalize_with_capability().unwrap();
        let mut bytes = std::fs::read(&summary.encrypted_path).unwrap();
        match malformed {
            4 => {
                bytes.truncate(bytes.len() - 29);
            } // Ended removed at whole chunk boundary.
            5 => {
                bytes.truncate(bytes.len() - 1);
            }
            6 => bytes.push(0),
            7 => {
                let last = bytes.len() - 1;
                bytes[last] ^= 1;
            }
            8 => bytes[12] ^= 1,
            _ => {}
        }
        std::fs::write(&summary.encrypted_path, &bytes).unwrap();
        // Sign the corrupted digest deliberately: a digest-only preflight would pass it.
        capability.sha256 = file_digest(&summary.encrypted_path).unwrap();
        recorder.key.authorize(&mut capability).unwrap();
        assert!(
            reconcile_finalized_recording(
                &fixture.settings,
                &capability,
                diarizer.clone(),
                CancellationToken::new()
            )
            .await
            .is_err(),
            "malformed case {malformed}"
        );
        fixture.assert_no_snapshot();
    }
    assert_eq!(diarizer.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn original_inode_mutation_between_preflight_and_provider_cannot_change_second_pass() {
    let fixture = Fixture::new();
    let mut recorder = fixture.recorder();
    for sequence in 0..100 {
        recorder.record_frame(&frame(sequence)).unwrap();
    }
    let (summary, capability) = recorder.finalize_with_capability().unwrap();
    let diarizer = Arc::new(Diarizer {
        mutate: Some(summary.encrypted_path),
        ..Diarizer::default()
    });
    reconcile_finalized_recording(
        &fixture.settings,
        &capability,
        diarizer.clone(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(diarizer.bytes.load(Ordering::SeqCst), 3_200_000);
    fixture.assert_no_snapshot();
}

#[test]
fn original_mutation_and_same_name_replacement_during_snapshot_verification_keep_original_bytes() {
    let fixture = Fixture::new();
    let mut recorder = fixture.recorder();
    recorder.record_frame(&frame(0)).unwrap();
    let (summary, capability) = recorder.finalize_with_capability().unwrap();
    let token = CancellationToken::new();
    let mut chunks = snapshot_finalized_recording(&fixture.settings, &capability, &token).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            chunks
                .reader
                .get_ref()
                .metadata()
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(chunks._snapshot.as_ref().unwrap().0.is_none());
    }
    std::fs::write(&summary.encrypted_path, b"same inode mutation").unwrap();
    std::fs::rename(
        &summary.encrypted_path,
        fixture.directory.path().join("replaced"),
    )
    .unwrap();
    std::fs::write(&summary.encrypted_path, b"replacement").unwrap();
    validate_completed_chunks(&mut chunks, &token).unwrap();
    chunks.reader.seek(SeekFrom::Start(0)).unwrap();
    assert_eq!(
        stream_digest(&mut chunks.reader, None, &token).unwrap(),
        capability.sha256
    );
    drop(chunks);
    fixture.assert_no_snapshot();
}

#[cfg(unix)]
#[test]
fn snapshot_process_crash_cannot_leave_a_named_ciphertext_copy() {
    const CHILD_ROOT: &str = "RUNTIME_SNAPSHOT_CRASH_TEST_ROOT";
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let root = PathBuf::from(root);
        std::env::set_var("RUNTIME_SNAPSHOT_CRASH_SYNTHETIC_KEY", KEY);
        let settings = RecordingSettings::new(
            root.join("recordings"),
            "RUNTIME_SNAPSHOT_CRASH_SYNTHETIC_KEY",
        )
        .unwrap();
        let mut recorder = SessionRecorder::start(&settings, SessionId::new(41)).unwrap();
        recorder.record_frame(&frame(0)).unwrap();
        let (_, capability) = recorder.finalize_with_capability().unwrap();
        let chunks =
            preflight_finalized_recording(&settings, &capability, &CancellationToken::new())
                .unwrap();
        assert!(chunks.reader.get_ref().metadata().is_ok());
        std::fs::write(root.join("ready"), b"snapshot descriptor held").unwrap();
        loop {
            std::thread::park();
        }
    }
    let root = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "session_recording::recovery_tests::snapshot_process_crash_cannot_leave_a_named_ciphertext_copy", "--nocapture"])
        .env(CHILD_ROOT, root.path()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).spawn().unwrap();
    let deadline = Instant::now() + std::time::Duration::from_secs(10);
    while !root.path().join("ready").exists() {
        if child.try_wait().unwrap().is_some() || Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "snapshot child did not reach held-descriptor phase: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let names = || {
        std::fs::read_dir(root.path().join("recordings"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>()
    };
    let before = names();
    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert!(!status.success());
    assert_eq!(names(), before);
    assert_eq!(before.len(), 1);
    assert!(before[0].to_string_lossy().ends_with(".crsr"));
}

#[tokio::test]
async fn empty_completed_recording_and_cancellation_have_honest_outcomes_and_cleanup() {
    let fixture = Fixture::new();
    let mut recorder = fixture.recorder();
    let (_, capability) = recorder.finalize_with_capability().unwrap();
    let diarizer = Arc::new(Diarizer::default());
    reconcile_finalized_recording(
        &fixture.settings,
        &capability,
        diarizer.clone(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(diarizer.bytes.load(Ordering::SeqCst), 0);
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(reconcile_finalized_recording(
        &fixture.settings,
        &capability,
        diarizer.clone(),
        cancelled
    )
    .await
    .is_err());
    assert_eq!(diarizer.calls.load(Ordering::SeqCst), 1);
    let mut recorder = fixture.recorder();
    for sequence in 0..100 {
        recorder.record_frame(&frame(sequence)).unwrap();
    }
    let (_, capability) = recorder.finalize_with_capability().unwrap();
    assert!(reconcile_finalized_recording(
        &fixture.settings,
        &capability,
        Arc::new(Diarizer {
            cancel: true,
            ..Diarizer::default()
        }),
        CancellationToken::new()
    )
    .await
    .is_err());
    fixture.assert_no_snapshot();
}

#[test]
fn failed_sync_or_incomplete_sink_cannot_issue_capability() {
    struct FailedSync(File);
    impl RecordingSink for FailedSync {
        fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
            Write::write_all(&mut self.0, bytes)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
        fn sync_all(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("synthetic sync failure"))
        }
    }
    let fixture = Fixture::new();
    let mut recorder = fixture.recorder();
    recorder.file = Some(Box::new(FailedSync(
        OpenOptions::new()
            .append(true)
            .open(recorder.path())
            .unwrap(),
    )));
    assert!(recorder.finalize_with_capability().is_err());
    assert!(recorder.summary.is_none());
    let mut recorder = fixture.recorder();
    recorder.mark_incomplete();
    assert!(recorder.finalize_with_capability().is_err());
}

#[test]
fn torn_sink_write_cannot_acquire_capability_by_retrying_finalization() {
    struct TornOnce {
        file: File,
        failed: bool,
    }
    impl RecordingSink for TornOnce {
        fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
            if !self.failed {
                self.failed = true;
                Write::write_all(&mut self.file, &bytes[..1])?;
                return Err(std::io::Error::other("synthetic torn encrypted write"));
            }
            Write::write_all(&mut self.file, bytes)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
        fn sync_all(&mut self) -> std::io::Result<()> {
            self.file.sync_all()
        }
    }
    let fixture = Fixture::new();
    let mut recorder = fixture.recorder();
    recorder.file = Some(Box::new(TornOnce {
        file: OpenOptions::new()
            .append(true)
            .open(recorder.path())
            .unwrap(),
        failed: false,
    }));
    assert!(recorder.finalize_with_capability().is_err());
    assert!(
        recorder.finalize_with_capability().is_err(),
        "a retry cannot authorize an earlier torn write"
    );
}

#[test]
fn externally_truncated_recording_cannot_acquire_a_finalized_capability() {
    let fixture = Fixture::new();
    let mut recorder = fixture.recorder();
    recorder.record_frame(&frame(0)).unwrap();
    OpenOptions::new()
        .write(true)
        .open(recorder.path())
        .unwrap()
        .set_len(HEADER_LEN as u64)
        .unwrap();
    assert!(recorder.finalize_with_capability().is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn oversized_sparse_replacement_is_rejected_before_snapshot_or_provider() {
    let fixture = Fixture::new();
    let mut recorder = fixture.recorder();
    let (summary, capability) = recorder.finalize_with_capability().unwrap();
    OpenOptions::new()
        .write(true)
        .open(&summary.encrypted_path)
        .unwrap()
        .set_len(8 * 1024 * 1024 * 1024 + 1)
        .unwrap();
    let guard = ForbidSnapshot::new(&summary.encrypted_path);
    let diarizer = Arc::new(Diarizer::default());
    let token = CancellationToken::new();
    assert!(reconcile_finalized_recording(
        &fixture.settings,
        &capability,
        diarizer.clone(),
        token.clone()
    )
    .await
    .is_err());
    token.cancel();
    assert!(
        reconcile_finalized_recording(&fixture.settings, &capability, diarizer.clone(), token)
            .await
            .is_err()
    );
    assert_eq!(diarizer.calls.load(Ordering::SeqCst), 0);
    fixture.assert_no_snapshot();
    assert_eq!(
        guard.1.load(Ordering::SeqCst),
        0,
        "oversized source entered snapshot allocation/copy"
    );
}

#[cfg(unix)]
#[test]
fn oversized_sparse_recording_cannot_reissue_a_cached_capability() {
    let fixture = Fixture::new();
    let mut fresh = fixture.recorder();
    OpenOptions::new()
        .write(true)
        .open(fresh.path())
        .unwrap()
        .set_len(8 * 1024 * 1024 * 1024 + 1)
        .unwrap();
    assert!(fresh.finalize_with_capability().is_err());
    assert!(
        !fresh.ended,
        "reject oversize before append, flush, or digest"
    );
    let mut recorder = fixture.recorder();
    recorder.finalize_with_capability().unwrap();
    OpenOptions::new()
        .write(true)
        .open(recorder.path())
        .unwrap()
        .set_len(8 * 1024 * 1024 * 1024 + 1)
        .unwrap();
    // Cached finalization makes this a real issuance RED without reading 8 GiB.
    assert!(recorder.finalize_with_capability().is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_to_valid_recording_is_not_followed_into_snapshot_allocation() {
    let fixture = Fixture::new();
    let mut recorder = fixture.recorder();
    let (summary, capability) = recorder.finalize_with_capability().unwrap();
    let saved = fixture.directory.path().join("saved-ciphertext");
    std::fs::rename(&summary.encrypted_path, &saved).unwrap();
    std::os::unix::fs::symlink(saved, &summary.encrypted_path).unwrap();
    let guard = ForbidSnapshot::new(&summary.encrypted_path);
    let diarizer = Arc::new(Diarizer::default());
    assert!(reconcile_finalized_recording(
        &fixture.settings,
        &capability,
        diarizer.clone(),
        CancellationToken::new()
    )
    .await
    .is_err());
    assert_eq!(diarizer.calls.load(Ordering::SeqCst), 0);
    fixture.assert_no_snapshot();
    assert_eq!(
        guard.1.load(Ordering::SeqCst),
        0,
        "symlink was followed into snapshot allocation"
    );
}

#[cfg(unix)]
#[test]
fn fifo_replacements_reject_and_join_after_cancellation_in_bounded_child() {
    const CHILD_ROOT: &str = "RUNTIME_RECOVERY_ADMISSION_CHILD_ROOT";
    const CHILD_KIND: &str = "RUNTIME_RECOVERY_ADMISSION_CHILD_KIND";
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let root = PathBuf::from(root);
        let mut fixture = Fixture::new();
        fixture.directory = tempfile::tempdir_in(&root).unwrap();
        fixture.settings =
            RecordingSettings::new(fixture.directory.path(), fixture.settings.key_env()).unwrap();
        let mut recorder = fixture.recorder();
        let (summary, capability) = recorder.finalize_with_capability().unwrap();
        std::fs::remove_file(&summary.encrypted_path).unwrap();
        let fifo = fixture.directory.path().join("fifo");
        let direct = std::env::var(CHILD_KIND).unwrap() == "direct";
        assert!(std::process::Command::new("mkfifo")
            .arg(if direct {
                &summary.encrypted_path
            } else {
                &fifo
            })
            .status()
            .unwrap()
            .success());
        if !direct {
            std::os::unix::fs::symlink(fifo, &summary.encrypted_path).unwrap();
        }
        let diarizer = Arc::new(Diarizer::default());
        let token = CancellationToken::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        std::fs::write(root.join("armed"), b"fixture ready; entering recovery").unwrap();
        let outcome = runtime.block_on(async {
            let recovery = reconcile_finalized_recording(
                &fixture.settings,
                &capability,
                diarizer.clone(),
                token.clone(),
            );
            tokio::pin!(recovery);
            tokio::select! {
                result = &mut recovery => return Some(result),
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => token.cancel(),
            }
            tokio::time::timeout(std::time::Duration::from_secs(1), &mut recovery)
                .await
                .ok()
        });
        assert_eq!(diarizer.calls.load(Ordering::SeqCst), 0);
        fixture.assert_no_snapshot();
        std::fs::write(
            root.join("outcome"),
            if matches!(outcome, Some(Err(_))) {
                "rejected"
            } else {
                "blocked"
            },
        )
        .unwrap();
        // On the unfixed implementation runtime drop would await the blocked open.
        // The parent owns the hard bound and always kills/reaps this child on failure.
        assert!(
            matches!(outcome, Some(Err(_))),
            "FIFO open did not finish after cancellation"
        );
        return;
    }
    for kind in ["direct", "symlink"] {
        let root = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "session_recording::recovery_tests::fifo_replacements_reject_and_join_after_cancellation_in_bounded_child", "--nocapture"])
            .env(CHILD_ROOT, root.path()).env(CHILD_KIND, kind)
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).spawn().unwrap();
        let startup_deadline = Instant::now() + std::time::Duration::from_secs(10);
        let mut phase_deadline = None;
        let mut success = false;
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                success = status.success();
                break;
            }
            if phase_deadline.is_none() && root.path().join("armed").exists() {
                phase_deadline = Some(Instant::now() + std::time::Duration::from_secs(2));
            }
            if std::fs::read(root.path().join("outcome")).is_ok_and(|bytes| bytes == b"blocked")
                || Instant::now() >= phase_deadline.unwrap_or(startup_deadline)
            {
                child.kill().unwrap();
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            success,
            "{kind} FIFO admission failed bounded rejection/join: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn digest_copy_is_bounded_and_hashes_the_exact_bytes_read_despite_in_place_source_mutation() {
    struct MutatingReader {
        file: File,
        writer: File,
        first: bool,
        max_read: usize,
    }
    impl Read for MutatingReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.max_read = self.max_read.max(buffer.len());
            let read = self.file.read(buffer)?;
            if self.first {
                self.first = false;
                self.writer.seek(SeekFrom::Start(0))?;
                Write::write_all(&mut self.writer, b"original inode changed after first read")?;
            }
            Ok(read)
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("encrypted");
    let mut writer = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&source)
        .unwrap();
    for _ in 0..256 {
        Write::write_all(&mut writer, &[0x55; DIGEST_BUFFER_BYTES]).unwrap();
    }
    let before = file_digest(&source).unwrap();
    let mut reader = MutatingReader {
        file: File::open(&source).unwrap(),
        writer,
        first: true,
        max_read: 0,
    };
    let snapshot_path = directory.path().join("snapshot");
    let mut snapshot = File::create(&snapshot_path).unwrap();
    let copied =
        stream_digest(&mut reader, Some(&mut snapshot), &CancellationToken::new()).unwrap();
    assert_eq!(copied, before);
    assert_ne!(file_digest(&source).unwrap(), before);
    assert_eq!(file_digest(&snapshot_path).unwrap(), copied);
    assert_eq!(reader.max_read, DIGEST_BUFFER_BYTES);
}

#[test]
fn gaps_beyond_the_summary_cap_keep_recording_and_stay_recoverable() {
    let fixture = Fixture::new();
    let mut recorder = fixture.recorder();
    let gaps = MAX_RECORDING_GAPS as u64 + 5;
    for sequence in 0..gaps {
        recorder.record_frame(&frame(sequence)).unwrap();
        recorder
            .record_capture_gap()
            .expect("a gap never stops the recording");
    }
    recorder.record_frame(&frame(gaps)).unwrap();
    let (summary, capability) = recorder.finalize_with_capability().unwrap();

    assert_eq!(summary.gaps.len(), MAX_RECORDING_GAPS);
    assert!(summary
        .gaps
        .iter()
        .all(|gap| gap.reason == CAPTURE_DISCONTINUITY_REASON && gap.ended_at_ms.is_some()));
    let coalesced = summary.gaps.last().unwrap();
    let frame_ms = frame_duration_ms(&frame(0));
    assert!(
        coalesced.started_at_ms >= (MAX_RECORDING_GAPS as u64) * frame_ms
            && coalesced.started_at_ms < (MAX_RECORDING_GAPS as u64 + 1) * frame_ms,
        "the last summary entry starts at the last gap that fit the cap: {coalesced:?}"
    );
    assert_eq!(
        coalesced.ended_at_ms,
        Some(summary.duration_ms - frame_ms),
        "and spans every later gap"
    );
    let mut chunks =
        preflight_finalized_recording(&fixture.settings, &capability, &CancellationToken::new())
            .expect("a recording with many gaps remains recoverable");
    let mut frames = 0;
    let mut gap_chunks = 0;
    while let Some(chunk) = chunks.next_chunk().unwrap() {
        match chunk {
            RecordedChunk::Frame { .. } => frames += 1,
            RecordedChunk::GapStarted { .. } => gap_chunks += 1,
            _ => {}
        }
    }
    assert_eq!(frames, gaps + 1, "every frame is recorded");
    assert_eq!(gap_chunks, gaps, "every gap stays exact in the file");
}

#[test]
fn a_gap_past_the_summary_cap_still_open_at_finalize_is_reported_open() {
    let fixture = Fixture::new();
    let mut recorder = fixture.recorder();
    for sequence in 0..=MAX_RECORDING_GAPS as u64 {
        recorder.record_frame(&frame(sequence)).unwrap();
        recorder.record_capture_gap().unwrap();
    }
    let summary = recorder.finalize().unwrap();
    assert_eq!(summary.gaps.len(), MAX_RECORDING_GAPS);
    assert_eq!(summary.gaps.last().unwrap().ended_at_ms, None);
    assert!(summary.gaps[..MAX_RECORDING_GAPS - 1]
        .iter()
        .all(|gap| gap.ended_at_ms.is_some()));
}
