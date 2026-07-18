use std::sync::{
    OnceLock,
    atomic::{AtomicU64, Ordering},
};

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

#[derive(Debug, Clone)]
pub(crate) struct GitToastEvent {
    pub(crate) command_id: u64,
    pub(crate) kind: GitToastEventKind,
}

#[derive(Debug, Clone)]
pub(crate) enum GitToastEventKind {
    Started {
        args: Vec<String>,
        timeout_secs: u64,
    },
    Finished {
        success: bool,
    },
    TimedOut {
        timeout_secs: u64,
    },
    Cancelled,
}

static EVENT_TX: OnceLock<UnboundedSender<GitToastEvent>> = OnceLock::new();

pub(crate) fn init_git_toast_channel() -> UnboundedReceiver<GitToastEvent> {
    let (tx, rx) = unbounded_channel();
    let _ = EVENT_TX.set(tx);
    rx
}

pub(crate) fn send_git_toast_event(event: GitToastEvent) {
    if let Some(tx) = EVENT_TX.get() {
        let _ = tx.send(event);
    }
}

static NEXT_COMMAND_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_git_command_id() -> u64 {
    NEXT_COMMAND_ID.fetch_add(1, Ordering::Relaxed)
}
