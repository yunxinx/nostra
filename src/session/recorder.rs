//! Thread-backed durable JSONL recorder.
//!
//! The public session capabilities are synchronous, but file I/O belongs to a
//! single worker that owns the `JsonlWriter`. This keeps append, retry and
//! shutdown ordering explicit without introducing another async runtime.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use super::{EntryId, JsonlWriter, SessionEntry, SessionEntryKind, SessionError};

const COMMAND_CAPACITY: usize = 64;
const DROP_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

type Response<T> = SyncSender<Result<T, SessionError>>;

enum RecorderCommand {
    Append {
        kinds: Vec<SessionEntryKind>,
        response: Response<Vec<SessionEntry>>,
    },
    SetLeaf {
        target: Option<EntryId>,
        response: Response<EntryId>,
    },
    Flush {
        response: Response<()>,
    },
    Shutdown {
        response: Response<()>,
    },
    #[cfg(test)]
    FailNextAppend {
        response: Response<()>,
    },
}

pub(crate) struct JsonlRecorder {
    sender: SyncSender<RecorderCommand>,
    worker: Option<JoinHandle<()>>,
    pending: Arc<AtomicBool>,
}

impl JsonlRecorder {
    pub(crate) fn create(
        path: impl Into<PathBuf>,
        header: super::SessionHeader,
    ) -> Result<Self, SessionError> {
        let path = path.into();
        let writer = JsonlWriter::create(&path, header)?;
        Self::spawn(writer)
    }

    pub(crate) fn open(path: impl Into<PathBuf>) -> Result<Self, SessionError> {
        let path = path.into();
        let writer = JsonlWriter::open(&path)?;
        Self::spawn(writer)
    }

    fn spawn(writer: JsonlWriter) -> Result<Self, SessionError> {
        let (sender, receiver) = mpsc::sync_channel(COMMAND_CAPACITY);
        let pending = Arc::new(AtomicBool::new(false));
        let worker_pending = Arc::clone(&pending);
        let worker = thread::Builder::new()
            .name("nostra-session-recorder".to_string())
            .spawn(move || run_worker(receiver, writer, worker_pending))
            .map_err(SessionError::io)?;
        Ok(Self {
            sender,
            worker: Some(worker),
            pending,
        })
    }

    pub(crate) fn append_batch(
        &self,
        kinds: Vec<SessionEntryKind>,
    ) -> Result<Vec<SessionEntry>, SessionError> {
        let (response, receiver) = self.command_response();
        self.sender
            .send(RecorderCommand::Append { kinds, response })
            .map_err(|_| worker_disconnected())?;
        receive_result(receiver)
    }

    pub(crate) fn set_leaf(&self, target: Option<&EntryId>) -> Result<EntryId, SessionError> {
        let (response, receiver) = self.command_response();
        self.sender
            .send(RecorderCommand::SetLeaf {
                target: target.cloned(),
                response,
            })
            .map_err(|_| worker_disconnected())?;
        receive_result(receiver)
    }

    pub(crate) fn flush(&self) -> Result<(), SessionError> {
        let (response, receiver) = self.command_response();
        self.sender
            .send(RecorderCommand::Flush { response })
            .map_err(|_| worker_disconnected())?;
        receive_result(receiver)
    }

    #[must_use]
    pub(crate) fn has_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }

    fn command_response<T>(&self) -> (Response<T>, Receiver<Result<T, SessionError>>) {
        let (response, receiver) = mpsc::sync_channel(1);
        (response, receiver)
    }

    #[cfg(test)]
    pub(crate) fn fail_next_append_for_test(&self) -> Result<(), SessionError> {
        let (response, receiver) = mpsc::sync_channel(1);
        self.sender
            .send(RecorderCommand::FailNextAppend { response })
            .map_err(|_| worker_disconnected())?;
        receiver.recv().map_err(|_| worker_disconnected())?
    }
}

fn receive_result<T>(receiver: Receiver<Result<T, SessionError>>) -> Result<T, SessionError> {
    receiver
        .recv()
        .map_err(|_| worker_disconnected())
        .and_then(|result| result)
}

impl Drop for JsonlRecorder {
    fn drop(&mut self) {
        let Some(worker) = self.worker.take() else {
            return;
        };
        let sender = self.sender.clone();
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let helper = thread::Builder::new()
            .name("nostra-session-recorder-shutdown".to_string())
            .spawn(move || {
                let (response, receiver) = mpsc::sync_channel(1);
                let result = sender
                    .send(RecorderCommand::Shutdown { response })
                    .ok()
                    .and_then(|()| receiver.recv().ok())
                    .and_then(Result::ok);
                let _ = worker.join();
                let _ = done_tx.send(result);
            });
        if helper.is_err() {
            // Dropping the worker handle detaches it. The sender/receiver
            // lifetime then lets the recorder finish queued work or exit
            // naturally without turning teardown into a panic.
            return;
        }
        // A recorder is dropped from UI-adjacent paths. Never make teardown
        // wait forever on a blocked filesystem operation. The helper thread
        // remains responsible for joining a worker that outlives the timeout.
        let _ = done_rx.recv_timeout(DROP_SHUTDOWN_TIMEOUT);
    }
}

fn run_worker(
    receiver: Receiver<RecorderCommand>,
    mut writer: JsonlWriter,
    pending_state: Arc<AtomicBool>,
) {
    let mut pending = Vec::new();
    #[cfg(test)]
    let mut fail_next_append = false;

    while let Ok(command) = receiver.recv() {
        match command {
            RecorderCommand::Append { kinds, response } => {
                let recovered = match persist_pending(&mut writer, &mut pending, &pending_state) {
                    Ok(entries) => entries,
                    Err(error) => {
                        // The new batch must remain pending too. It has not been
                        // attempted and is part of the exact durable work queue.
                        pending.extend(kinds);
                        pending_state.store(true, Ordering::Release);
                        let _ = response.send(Err(error));
                        continue;
                    }
                };
                {
                    #[cfg(test)]
                    if fail_next_append {
                        fail_next_append = false;
                        pending = kinds;
                        pending_state.store(true, Ordering::Release);
                        let _ = response.send(Err(SessionError::io(std::io::Error::other(
                            "injected JSONL write failure",
                        ))));
                        continue;
                    }
                    let result = match writer.append_batch_entries(kinds.clone()) {
                        Ok(entries) => {
                            let mut all = recovered;
                            all.extend(entries);
                            Ok(all)
                        }
                        Err(error) => {
                            pending = kinds;
                            pending_state.store(true, Ordering::Release);
                            Err(error)
                        }
                    };
                    let _ = response.send(result);
                }
            }
            RecorderCommand::SetLeaf { target, response } => {
                let result = persist_pending(&mut writer, &mut pending, &pending_state)
                    .and_then(|_| writer.set_leaf(target.as_ref()));
                let _ = response.send(result);
            }
            RecorderCommand::Flush { response } => {
                let result = persist_pending(&mut writer, &mut pending, &pending_state)
                    .and_then(|_| writer.flush());
                let _ = response.send(result);
            }
            RecorderCommand::Shutdown { response } => {
                let result = persist_pending(&mut writer, &mut pending, &pending_state)
                    .and_then(|_| writer.flush());
                let _ = response.send(result);
                return;
            }
            #[cfg(test)]
            RecorderCommand::FailNextAppend { response } => {
                fail_next_append = true;
                let _ = response.send(Ok(()));
            }
        }
    }

    let _ = persist_pending(&mut writer, &mut pending, &pending_state);
}

fn persist_pending(
    writer: &mut JsonlWriter,
    pending: &mut Vec<SessionEntryKind>,
    pending_state: &AtomicBool,
) -> Result<Vec<SessionEntry>, SessionError> {
    if pending.is_empty() {
        pending_state.store(false, Ordering::Release);
        return Ok(Vec::new());
    }
    let batch = std::mem::take(pending);
    match writer.append_batch_entries(batch.clone()) {
        Ok(entries) => {
            pending_state.store(false, Ordering::Release);
            Ok(entries)
        }
        Err(error) => {
            *pending = batch;
            pending_state.store(true, Ordering::Release);
            Err(error)
        }
    }
}

fn worker_disconnected() -> SessionError {
    SessionError::io(std::io::Error::other(
        "session recorder worker disconnected",
    ))
}
