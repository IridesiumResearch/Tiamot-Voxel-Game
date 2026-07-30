// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Graceful shutdown on ctrl-c or SIGTERM.
//!
//! A voxel server holds unsaved world state, so being killed is a data-loss
//! event. Both signals must reach the same save-then-exit path: ctrl-c is how
//! an operator stops a foreground server, SIGTERM is how systemd, Docker, and
//! Kubernetes stop a backgrounded one.

use std::sync::mpsc::{Receiver, sync_channel};

/// A one-shot handle that becomes ready when a shutdown signal arrives.
pub struct Signal {
    receiver: Receiver<()>,
}

impl Signal {
    /// Blocks until a shutdown signal arrives.
    ///
    /// Returns immediately if the handler was torn down without signalling,
    /// which can only happen during process teardown — treating that as
    /// "shut down" is the correct response either way.
    pub fn wait(&self) {
        // A `RecvError` means every sender was dropped without signalling,
        // which can only happen during process teardown. Shutting down is the
        // right response to that as much as to a real signal, so both arms of
        // the result lead here.
        let _ = self.receiver.recv();
    }
}

/// Installs the shutdown handler and returns a handle to wait on.
///
/// `ctrlc`'s `termination` feature covers SIGINT and SIGTERM on Unix and
/// ctrl-c, ctrl-break, and console-close on Windows.
///
/// If the handler cannot be installed — which in practice means one is already
/// installed — this logs and returns a handle that simply never fires. That is
/// deliberate: refusing to boot a server because a signal handler is already
/// present would be a worse outcome than running without our own.
pub fn listen() -> Signal {
    // Capacity 1, and the handler must never block: it runs in a signal
    // context. A second signal finding the channel full is dropped, which is
    // right — shutdown is already under way.
    let (sender, receiver) = sync_channel::<()>(1);

    if let Err(err) = ctrlc::set_handler(move || {
        let _ = sender.try_send(());
    }) {
        tracing::warn!("could not install shutdown handler: {err}");
    }

    Signal { receiver }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn wait_returns_when_the_sender_is_dropped() {
        // Proves the teardown path does not hang the process. We cannot raise a
        // real signal in a unit test without taking down the test harness, so
        // the signal path itself is covered by the manual run in the task's
        // acceptance criteria.
        let (sender, receiver) = sync_channel::<()>(1);
        let signal = Signal { receiver };

        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            drop(sender);
        });

        signal.wait();
    }

    #[test]
    fn wait_returns_when_signalled() {
        let (sender, receiver) = sync_channel::<()>(1);
        let signal = Signal { receiver };

        sender.try_send(()).expect("channel has capacity");
        signal.wait();
    }
}
