//! OS-level camera hotplug detection.
//!
//! The UI doesn't want a Refresh button — it wants to know when the
//! list of capture devices changes so it can re-run [`crate::cameras::list`].
//! [`Watcher`] is a tiny "something happened" signal: the consumer
//! drains [`Watcher::changed`] each frame and, if anything came through,
//! re-enumerates.
//!
//! ## Platform strategies
//!
//! - **macOS**: register an observer on `NSNotificationCenter` for
//!   `AVCaptureDeviceWasConnectedNotification` /
//!   `AVCaptureDeviceWasDisconnectedNotification`. AVFoundation delivers
//!   the block on its own GCD worker thread (we register with
//!   `queue: nil`), so the registering thread is free to park.
//! - **Other platforms** (Linux / Windows): slow re-poll thread that
//!   calls [`crate::cameras::list`] every two seconds and only signals
//!   when the result actually differs from the previous snapshot. This
//!   is "good enough" until we have time to wire `udev` /
//!   `RegisterDeviceNotification`.

use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

/// Handle for hotplug detection. Owns the worker thread (currently
/// detached; we keep the handle as a single-purpose Receiver wrapper so
/// the API can grow later without breaking callers).
pub struct Watcher {
    rx: Receiver<()>,
}

impl Watcher {
    /// Returns true if at least one device-change signal arrived since
    /// the last call. Drains every pending signal so a burst (e.g. a
    /// dock that flips four cameras on at once) coalesces into a single
    /// re-enumeration.
    pub fn changed(&mut self) -> bool {
        let mut any = false;
        loop {
            match self.rx.try_recv() {
                Ok(()) => any = true,
                Err(TryRecvError::Empty) => return any,
                // Worker thread died — no more signals will ever come.
                // Treat as "no change"; the caller will keep working
                // off whatever the last enumeration produced.
                Err(TryRecvError::Disconnected) => return any,
            }
        }
    }
}

/// Start the platform-specific watcher. Never errors loudly — if we
/// can't set up notifications on a given OS we fall back to a slow
/// re-poll worker, and if even that fails the [`Watcher`] simply never
/// fires (matching the pre-hotplug behaviour with the manual button
/// removed).
pub fn watch() -> Watcher {
    let (tx, rx) = mpsc::channel();
    spawn_platform_worker(tx);
    Watcher { rx }
}

#[cfg(target_os = "macos")]
fn spawn_platform_worker(tx: Sender<()>) {
    macos::spawn(tx);
}

#[cfg(not(target_os = "macos"))]
fn spawn_platform_worker(tx: Sender<()>) {
    spawn_poll_worker(tx);
}

/// Cross-platform fallback: poll `cameras::list()` on a two-second
/// cadence and signal only when the device set differs from the
/// previous snapshot. Cheap enough on Linux (sysfs read); on Windows
/// the dshow subprocess is heavier so we keep the interval generous.
#[cfg(not(target_os = "macos"))]
fn spawn_poll_worker(tx: Sender<()>) {
    use std::thread;
    use std::time::Duration;

    thread::Builder::new()
        .name("camera-hotplug-poll".to_string())
        .spawn(move || {
            let mut last = crate::cameras::list().unwrap_or_default();
            loop {
                thread::sleep(Duration::from_secs(2));
                let next = match crate::cameras::list() {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if next != last {
                    last = next;
                    // Receiver dropped — nothing left to do.
                    if tx.send(()).is_err() {
                        return;
                    }
                }
            }
        })
        .ok();
}

#[cfg(target_os = "macos")]
mod macos {
    //! AVFoundation hotplug via `NSNotificationCenter`.
    //!
    //! Apple posts `AVCaptureDeviceWasConnectedNotification` and
    //! `AVCaptureDeviceWasDisconnectedNotification` whenever the
    //! capture-device set changes (USB cameras, Continuity Camera
    //! iPhones, virtual cameras, etc.). Subscribing is just
    //! `NSNotificationCenter.defaultCenter.addObserverForName:...`.
    //!
    //! ## Threading
    //!
    //! We register with `queue: nil`, which tells AVFoundation to
    //! dispatch the block on an arbitrary GCD worker thread — *not*
    //! the thread that called `addObserverForName:`. That means the
    //! thread we spin up here has no real work to do after
    //! registration: the notification center retains the block, the
    //! observer token is leaked into Cocoa's retain pool, and
    //! deliveries happen on GCD. The thread then just `park()`s
    //! forever so it (and anything it owns) sticks around for the
    //! life of the process. A future cleanup could register on the
    //! main thread and skip the spawn entirely.
    //!
    //! ## Safety
    //!
    //! We never touch the AVCaptureDevice class itself — we only ask
    //! `NSNotificationCenter` to call us back when notifications with
    //! known names appear. That means no AVFoundation linkage and no
    //! permission prompt (TCC isn't triggered until you actually open
    //! a device). The observer block captures a cloned `Sender` and
    //! is retained by the notification center for the life of the
    //! process.

    use std::ptr::NonNull;
    use std::sync::mpsc::Sender;
    use std::thread;

    use block2::RcBlock;
    use objc2_foundation::{
        NSNotification, NSNotificationCenter, NSNotificationName, NSString,
    };

    pub(super) fn spawn(tx: Sender<()>) {
        thread::Builder::new()
            .name("camera-hotplug-macos".to_string())
            .spawn(move || run(tx))
            .ok();
    }

    fn run(tx: Sender<()>) {
        // NSNotificationCenter and the singleton NSStrings are
        // thread-safe; we set everything up here and never touch
        // these objects from other threads.
        let center = NSNotificationCenter::defaultCenter();

        // The notification names are documented Cocoa string
        // constants — passing the raw name as an NSString avoids
        // linking AVFoundation just to read two symbols.
        let connected = NSString::from_str("AVCaptureDeviceWasConnectedNotification");
        let disconnected = NSString::from_str("AVCaptureDeviceWasDisconnectedNotification");

        // `Sender<()>` is `Clone`, so each block can own its own
        // copy — no Mutex needed. Sends from different GCD threads
        // are independently safe because the channel itself is
        // internally synchronised.
        register(&center, &connected, tx.clone());
        register(&center, &disconnected, tx);

        // The blocks are delivered on GCD worker threads (we passed
        // `queue: nil`), not on this thread, so there's nothing for
        // a run loop to drain here. We just park forever to keep
        // this thread alive — and with it the observer registration,
        // which is already leaked into Cocoa's retain pool. The
        // `loop` guards against spurious unparks. `park()` is the
        // right primitive: zero CPU, zero wakeups under normal
        // operation, and the OS can suspend the thread entirely.
        loop {
            thread::park();
        }
    }

    /// Register an observer block that sends `()` on `tx` every time
    /// a notification with `name` is posted. `nil` object = match any
    /// sender; `nil` queue = AVFoundation picks a GCD worker thread.
    /// The send is non-blocking so doing it inline is fine.
    fn register(center: &NSNotificationCenter, name: &NSNotificationName, tx: Sender<()>) {
        // The notification center retains the block for the life of
        // the registration; `RcBlock::new` heap-allocates and the
        // center bumps the refcount on its own copy. We then drop
        // our handle — the block stays alive inside Cocoa.
        let block = RcBlock::new(move |_note: NonNull<NSNotification>| {
            // Receiver gone → app is shutting down; silently drop.
            let _ = tx.send(());
        });

        // addObserverForName:object:queue:usingBlock: returns an
        // opaque token used to *remove* the observer. We register
        // for the life of the process, so we let the token leak
        // into the notification center's retain pool.
        //
        // SAFETY: name is a non-null NSString; obj and queue are nil
        // (allowed by the API contract); the block is a valid
        // `Fn(NonNull<NSNotification>)` with a 'static closure.
        let _token = unsafe {
            center.addObserverForName_object_queue_usingBlock(Some(name), None, None, &block)
        };
    }
}
