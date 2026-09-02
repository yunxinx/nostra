use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const FLUSH_INTERVAL: Duration = Duration::from_secs(2);
const FLUSH_BATCH_SIZE: usize = 32;
const WRITE_BUFFER_BYTES: usize = super::LOG_RECORD_MAX_BYTES * FLUSH_BATCH_SIZE;

pub(super) struct LogConfig {
    pub(super) path: PathBuf,
    pub(super) queue_capacity: usize,
    pub(super) max_bytes: u64,
    pub(super) backups: usize,
}

enum Command {
    Record {
        line: String,
        flush_immediately: bool,
    },
}

enum WorkerEvent {
    Command(Command),
    FlushDeadline,
    Disconnected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EnqueueError {
    Full,
    Disconnected,
}

struct WorkerState {
    closed: AtomicBool,
    finished: Mutex<bool>,
    finished_cv: Condvar,
    join: Mutex<Option<JoinHandle<()>>>,
}

pub(super) struct LogHandle {
    sender: Mutex<Option<SyncSender<Command>>>,
    state: Arc<WorkerState>,
}

impl LogHandle {
    pub(super) fn start(config: LogConfig) -> io::Result<Self> {
        if config.queue_capacity == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "log queue capacity must be positive",
            ));
        }
        if config.max_bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "log size limit must be positive",
            ));
        }
        let parent = config
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        // Open once before spawning so an unwritable path is reported during
        // initialization and the worker receives the already-open sink.
        let writer = LogWriter::open(&config.path)?;

        let (sender, receiver) = mpsc::sync_channel(config.queue_capacity);
        let state = Arc::new(WorkerState {
            closed: AtomicBool::new(false),
            finished: Mutex::new(false),
            finished_cv: Condvar::new(),
            join: Mutex::new(None),
        });
        let worker_state = Arc::clone(&state);
        let join = thread::Builder::new()
            .name("nostra-log-writer".into())
            .spawn(move || run_worker(receiver, config, writer, worker_state))?;
        *state
            .join
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(join);

        Ok(Self {
            sender: Mutex::new(Some(sender)),
            state,
        })
    }

    pub(super) fn try_record(
        &self,
        line: String,
        flush_immediately: bool,
    ) -> Result<(), EnqueueError> {
        let sender = self
            .sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(sender) = sender.as_ref() else {
            return Err(EnqueueError::Disconnected);
        };
        if self.state.closed.load(Ordering::Acquire) {
            return Err(EnqueueError::Disconnected);
        }
        let line = super::encode_record(line);
        sender
            .try_send(Command::Record {
                line,
                flush_immediately,
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => EnqueueError::Full,
                TrySendError::Disconnected(_) => EnqueueError::Disconnected,
            })
    }

    pub(super) fn shutdown(&self) {
        if self.state.closed.swap(true, Ordering::AcqRel) {
            return;
        }

        // Dropping the sender lets the worker drain already accepted records,
        // then observe Disconnected and flush before it exits. No blocking send
        // is performed here, so a full queue cannot hang application quit.
        self.sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();

        let mut finished = self
            .state
            .finished
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let deadline = std::time::Instant::now() + SHUTDOWN_TIMEOUT;
        while !*finished {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let (next, timeout) = self
                .state
                .finished_cv
                .wait_timeout(finished, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            finished = next;
            if timeout.timed_out() {
                break;
            }
        }
        let completed = *finished;
        drop(finished);

        let join = self
            .state
            .join
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(join) = join {
            if completed {
                let _ = join.join();
            } else {
                // A stuck filesystem call must not keep the UI process alive.
                // Dropping JoinHandle detaches the worker; it owns no product
                // state and will terminate when the blocked operation returns.
                eprintln!("nostra logger did not stop within five seconds; detaching worker");
                drop(join);
            }
        }
    }
}

impl Drop for LogHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run_worker(
    receiver: Receiver<Command>,
    config: LogConfig,
    mut writer: LogWriter,
    state: Arc<WorkerState>,
) {
    let mut unflushed_records = 0_usize;
    let mut batch_started_at = None;
    let mut writer_failed = false;

    loop {
        // With no buffered records, block until work arrives. Once a batch
        // starts, its two-second deadline is measured from the first record,
        // not from worker startup or the previous flush. This avoids idle
        // polling and gives sparse diagnostics a real batching opportunity.
        match next_worker_event(&receiver, batch_started_at) {
            WorkerEvent::Command(Command::Record {
                line,
                flush_immediately,
            }) => {
                if let Err(error) = writer.append(&config, &line) {
                    eprintln!(
                        "nostra logger failed to write {}: {error}",
                        config.path.display()
                    );
                    writer_failed = true;
                    break;
                }
                unflushed_records = unflushed_records.saturating_add(1);
                batch_started_at.get_or_insert_with(Instant::now);
                if flush_immediately || unflushed_records >= FLUSH_BATCH_SIZE {
                    if let Err(error) = writer.flush() {
                        eprintln!(
                            "nostra logger failed to flush {}: {error}",
                            config.path.display()
                        );
                        writer_failed = true;
                        break;
                    }
                    unflushed_records = 0;
                    batch_started_at = None;
                }
            }
            WorkerEvent::FlushDeadline => {
                if unflushed_records > 0 {
                    if let Err(error) = writer.flush() {
                        eprintln!(
                            "nostra logger failed to flush {}: {error}",
                            config.path.display()
                        );
                        writer_failed = true;
                        break;
                    }
                    unflushed_records = 0;
                    batch_started_at = None;
                }
            }
            WorkerEvent::Disconnected => break,
        }
    }

    if !writer_failed && let Err(error) = writer.flush() {
        eprintln!(
            "nostra logger failed to flush {}: {error}",
            config.path.display()
        );
    }
    mark_finished(&state);
}

fn next_worker_event(
    receiver: &Receiver<Command>,
    batch_started_at: Option<Instant>,
) -> WorkerEvent {
    let Some(batch_started_at) = batch_started_at else {
        return match receiver.recv() {
            Ok(command) => WorkerEvent::Command(command),
            Err(_) => WorkerEvent::Disconnected,
        };
    };
    let wait = FLUSH_INTERVAL.saturating_sub(batch_started_at.elapsed());
    match receiver.recv_timeout(wait) {
        Ok(command) => WorkerEvent::Command(command),
        Err(mpsc::RecvTimeoutError::Timeout) => WorkerEvent::FlushDeadline,
        Err(mpsc::RecvTimeoutError::Disconnected) => WorkerEvent::Disconnected,
    }
}

fn mark_finished(state: &WorkerState) {
    let mut finished = state
        .finished
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *finished = true;
    state.finished_cv.notify_all();
}

struct LogWriter {
    file: Option<BufWriter<File>>,
    bytes: u64,
}

impl LogWriter {
    fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let bytes = file.metadata()?.len();
        Ok(Self {
            // One batch of maximum-sized records fits in memory, so the
            // worker's 32-record policy remains a real write boundary instead
            // of being defeated by BufWriter's much smaller default capacity.
            file: Some(BufWriter::with_capacity(WRITE_BUFFER_BYTES, file)),
            bytes,
        })
    }

    fn append(&mut self, config: &LogConfig, line: &str) -> io::Result<()> {
        let formatted_len = line.len() as u64;
        if self.bytes > 0 && self.bytes.saturating_add(formatted_len) > config.max_bytes {
            self.flush()?;
            // Close the active file before renaming it so rotation also works
            // on platforms that reject renaming open files.
            drop(self.file.take());
            rotate(&config.path, config.backups)?;
            *self = Self::open(&config.path)?;
        }
        self.file_mut()?.write_all(line.as_bytes())?;
        self.bytes = self.bytes.saturating_add(formatted_len);
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file_mut()?.flush()
    }

    fn file_mut(&mut self) -> io::Result<&mut BufWriter<File>> {
        self.file.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "diagnostic log file is not open")
        })
    }
}

fn rotate(path: &Path, backups: usize) -> io::Result<()> {
    if backups == 0 {
        File::create(path)?;
        return Ok(());
    }
    for index in (1..=backups).rev() {
        let source = if index == 1 {
            path.to_path_buf()
        } else {
            backup_path(path, index - 1)
        };
        if !source.exists() {
            continue;
        }
        let destination = backup_path(path, index);
        if destination.exists() {
            fs::remove_file(&destination)?;
        }
        fs::rename(source, destination)?;
    }
    File::create(path)?;
    Ok(())
}

pub(super) fn backup_path(path: &Path, index: usize) -> PathBuf {
    let mut backup = path.as_os_str().to_os_string();
    backup.push(format!(".{index}"));
    PathBuf::from(backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_queue_reports_full_without_waiting_for_a_consumer() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let logger = LogHandle {
            sender: Mutex::new(Some(sender)),
            state: Arc::new(WorkerState {
                closed: AtomicBool::new(false),
                // No worker exists in this focused queue test.
                finished: Mutex::new(true),
                finished_cv: Condvar::new(),
                join: Mutex::new(None),
            }),
        };

        assert_eq!(logger.try_record("first".into(), false), Ok(()));
        assert_eq!(
            logger.try_record("second".into(), false),
            Err(EnqueueError::Full)
        );

        drop(receiver);
        logger.shutdown();
    }
}
