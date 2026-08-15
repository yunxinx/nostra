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
    Flush {
        response: Response<()>,
    },
    Shutdown {
        response: Response<()>,
    },
    Abort {
        response: Response<()>,
    },
    #[cfg(test)]
    FailNextAppend {
        response: Response<()>,
    },
    #[cfg(test)]
    FailNextAppendAfterWrite {
        response: Response<()>,
    },
}

pub(crate) struct JsonlRecorder {
    sender: SyncSender<RecorderCommand>,
    worker: Option<JoinHandle<()>>,
    pending: Arc<AtomicBool>,
    persist_on_disconnect: Arc<AtomicBool>,
}

impl JsonlRecorder {
    pub(crate) fn open(path: impl Into<PathBuf>) -> Result<Self, SessionError> {
        let path = path.into();
        let writer = JsonlWriter::open(&path)?;
        Self::spawn(writer)
    }

    fn spawn(writer: JsonlWriter) -> Result<Self, SessionError> {
        let (sender, receiver) = mpsc::sync_channel(COMMAND_CAPACITY);
        let pending = Arc::new(AtomicBool::new(false));
        let worker_pending = Arc::clone(&pending);
        let persist_on_disconnect = Arc::new(AtomicBool::new(true));
        let worker_persist_on_disconnect = Arc::clone(&persist_on_disconnect);
        let worker = thread::Builder::new()
            .name("nostra-session-recorder".to_string())
            .spawn(move || {
                run_worker(
                    receiver,
                    writer,
                    worker_pending,
                    worker_persist_on_disconnect,
                )
            })
            .map_err(SessionError::io)?;
        Ok(Self {
            sender,
            worker: Some(worker),
            pending,
            persist_on_disconnect,
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
        let mut entries = self.append_batch(vec![SessionEntryKind::Leaf(super::Leaf {
            target_id: target.cloned(),
        })])?;
        entries.pop().map(|entry| entry.id).ok_or_else(|| {
            SessionError::io(std::io::Error::other(
                "session recorder returned no Leaf entry",
            ))
        })
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

    pub(crate) fn abandon_pending_after_authority_loss(&mut self) {
        // The caller has proved that this path is no longer an authorized
        // store capability. Retrying through the lexical name during Drop
        // could follow a replacement symlink and disclose the exact batch.
        self.persist_on_disconnect.store(false, Ordering::Release);
        self.pending.store(false, Ordering::Release);
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

    #[cfg(test)]
    pub(crate) fn fail_next_append_after_write_for_test(&self) -> Result<(), SessionError> {
        let (response, receiver) = mpsc::sync_channel(1);
        self.sender
            .send(RecorderCommand::FailNextAppendAfterWrite { response })
            .map_err(|_| worker_disconnected())?;
        receiver.recv().map_err(|_| worker_disconnected())?
    }

    #[cfg(test)]
    pub(crate) fn fail_next_set_leaf_after_write_for_test(&self) -> Result<(), SessionError> {
        self.fail_next_append_after_write_for_test()
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
        let persist = self.persist_on_disconnect.load(Ordering::Acquire);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let helper = thread::Builder::new()
            .name("nostra-session-recorder-shutdown".to_string())
            .spawn(move || {
                let (response, receiver) = mpsc::sync_channel(1);
                let command = if persist {
                    RecorderCommand::Shutdown { response }
                } else {
                    RecorderCommand::Abort { response }
                };
                let result = sender
                    .send(command)
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
    persist_on_disconnect: Arc<AtomicBool>,
) {
    let mut pending: Vec<SessionEntry> = Vec::new();
    #[cfg(test)]
    let mut fail_next_append = false;
    #[cfg(test)]
    let mut fail_next_append_after_write = false;

    while let Ok(command) = receiver.recv() {
        match command {
            RecorderCommand::Append { kinds, response } => {
                let prepared = match writer.prepare_batch_after(kinds, &pending) {
                    Ok(entries) => entries,
                    Err(error) => {
                        let _ = response.send(Err(error));
                        continue;
                    }
                };
                let recovered = match persist_pending(&mut writer, &mut pending, &pending_state) {
                    Ok(entries) => entries,
                    Err(error) => {
                        // The current request was validated and assigned stable
                        // identity against the pending tail before the retry.
                        // Keep that exact batch queued behind the older one.
                        pending.extend(prepared);
                        pending_state.store(true, Ordering::Release);
                        let _ = response.send(Err(error));
                        continue;
                    }
                };
                {
                    #[cfg(test)]
                    if fail_next_append {
                        fail_next_append = false;
                        pending = prepared;
                        pending_state.store(true, Ordering::Release);
                        let _ = response.send(Err(SessionError::io(std::io::Error::other(
                            "injected JSONL write failure",
                        ))));
                        continue;
                    }
                    let result = match writer.append_entries(&prepared) {
                        Ok(()) => {
                            #[cfg(test)]
                            if fail_next_append_after_write {
                                fail_next_append_after_write = false;
                                pending = prepared;
                                pending_state.store(true, Ordering::Release);
                                let _ =
                                    response.send(Err(SessionError::io(std::io::Error::other(
                                        "injected JSONL result loss after durable write",
                                    ))));
                                continue;
                            }
                            let mut all = recovered;
                            all.extend(prepared);
                            Ok(all)
                        }
                        Err(error) => {
                            pending = prepared;
                            pending_state.store(true, Ordering::Release);
                            Err(error)
                        }
                    };
                    let _ = response.send(result);
                }
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
            RecorderCommand::Abort { response } => {
                pending.clear();
                pending_state.store(false, Ordering::Release);
                let _ = response.send(Ok(()));
                return;
            }
            #[cfg(test)]
            RecorderCommand::FailNextAppend { response } => {
                fail_next_append = true;
                let _ = response.send(Ok(()));
            }
            #[cfg(test)]
            RecorderCommand::FailNextAppendAfterWrite { response } => {
                fail_next_append_after_write = true;
                let _ = response.send(Ok(()));
            }
        }
    }

    if persist_on_disconnect.load(Ordering::Acquire) {
        let _ = persist_pending(&mut writer, &mut pending, &pending_state);
    }
}

fn persist_pending(
    writer: &mut JsonlWriter,
    pending: &mut Vec<SessionEntry>,
    pending_state: &AtomicBool,
) -> Result<Vec<SessionEntry>, SessionError> {
    if pending.is_empty() {
        pending_state.store(false, Ordering::Release);
        return Ok(Vec::new());
    }
    let batch = std::mem::take(pending);
    match writer.reconcile_entries(&batch) {
        Ok(()) => {
            pending_state.store(false, Ordering::Release);
            Ok(batch)
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
