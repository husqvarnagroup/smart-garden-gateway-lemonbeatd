// SPDX-FileCopyrightText: GARDENA GmbH
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Provides [AbortableSleep].

/// Shared state between sleeper and aborter.
struct AbortableSleepInner {
    abort_handle: Option<futures::future::AbortHandle>,
    should_abort: bool,
    dropped: bool,
}

/// Aborter handle.
///
/// An abortable sleep is exactly what it sounds like. You have a task doing an
/// async sleep and you have a handle that another task can use to interrupt
/// that sleep.
///
/// The implementation is slightly more complex than what one might think to
/// address several race conditions. Sleep also gets aborted if the
/// `AbortableSleep` handle gets dropped. The abort reason is provided as well.
///
/// Downside of the current implementation: We allow having multiple
/// abort handles even though that doesn't make sense and would result in
/// unexpected behavior.
pub struct AbortableSleep {
    inner: std::sync::Arc<std::sync::Mutex<AbortableSleepInner>>,
}

impl AbortableSleep {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Arc::new(std::sync::Mutex::new(AbortableSleepInner {
                abort_handle: None,
                should_abort: false,
                dropped: false,
            })),
        }
    }

    pub fn abort(&self) {
        let mut inner = self.inner.lock().unwrap();

        // if it doesn't exist it's not sleeping and thus there's no need to
        // abort it ...
        // ... unless the waiting time got reduced before the task had a chance
        // to sleep. In that case, we remember that an abort request occurred
        // which will cause the next sleep to return immediately.
        if let Some(handle) = inner.abort_handle.as_ref() {
            handle.abort();
        } else {
            inner.should_abort = true;
        }
    }

    pub fn handle(&self) -> AbortableSleepHandle {
        AbortableSleepHandle {
            inner: self.inner.clone(),
        }
    }
}

impl Drop for AbortableSleep {
    fn drop(&mut self) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.dropped = true;
        }

        self.abort();
    }
}

pub enum SleepResult {
    Interrupted,
    Slept,
    Dropped,
}

/// Sleeper handle.
///
/// This is used by the sleeper task.
pub struct AbortableSleepHandle {
    inner: std::sync::Arc<std::sync::Mutex<AbortableSleepInner>>,
}

impl AbortableSleepHandle {
    pub async fn sleep(&self, duration: std::time::Duration) -> SleepResult {
        let future = {
            let mut inner = self.inner.lock().unwrap();
            if inner.dropped {
                return SleepResult::Dropped;
            }
            if inner.should_abort {
                inner.should_abort = false;
                return SleepResult::Interrupted;
            }

            let sleep = tokio::time::sleep(duration);
            let (abort_handle, abort_registration) = futures::future::AbortHandle::new_pair();
            let future = futures::future::Abortable::new(sleep, abort_registration);

            inner.abort_handle = Some(abort_handle);

            future
        };

        match future.await {
            Ok(_) => SleepResult::Slept,
            Err(_) => SleepResult::Interrupted,
        }
    }
}
