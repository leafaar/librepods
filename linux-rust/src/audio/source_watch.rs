//! Who records from the hi-res virtual source, kept current over one sound
//! server connection. Every new client makes PipeWire's pulse server bind and
//! enumerate the whole graph, so the watch connects once, subscribes to source
//! output events and reads the recorders again only when one changed.

use {
    crate::audio::{
        output::{SourceConsumers, source_consumers_on},
        pulse::{SoundServerError, connect_cancellable, wait_for},
    },
    libpulse_binding::{
        context::{
            Context, State as ContextState,
            subscribe::{Facility, InterestMaskSet},
        },
        mainloop::standard::Mainloop,
        time::MicroSeconds,
    },
    std::{
        cell::Cell,
        io,
        rc::Rc,
        thread::JoinHandle,
        time::{Duration, Instant},
    },
    tokio::sync::watch,
    tracing::{debug, warn},
};

/// Longest the watch thread waits for the sound server (200 ms) before it
/// checks whether anyone still listens. Bounds how long close() waits.
const WAIT_STEP: MicroSeconds = MicroSeconds(200_000);
/// Delay before the first reconnect after the connection dropped or could not
/// be opened; every further failure in a row doubles it, up to
/// RECONNECT_MAX_DELAY.
const RECONNECT_BASE_DELAY: Duration = Duration::from_secs(1);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(30);

/// The recorders of one source, kept current by a background thread that owns
/// the sound server connection. The libpulse mainloop is not Send, so it
/// cannot live on the async runtime.
pub struct SourceWatch {
    consumers: watch::Receiver<SourceConsumers>,
    thread: JoinHandle<()>,
}

impl SourceWatch {
    pub fn start(source: &'static str) -> io::Result<SourceWatch> {
        let (tx, consumers) = watch::channel(SourceConsumers::default());
        let thread = std::thread::Builder::new()
            .name("hires-watch".into())
            .spawn(move || {
                watch_recorders(
                    &tx,
                    || PulseRecorders::connect(source, || tx.is_closed()),
                    |delay| pause_unless_closed(&tx, delay),
                );
            })?;
        Ok(SourceWatch { consumers, thread })
    }

    /// The latest recorders. A failed read or a dropped connection keeps the
    /// last answer: one missed reading must not look like the recorder leaving.
    pub fn consumers(&mut self) -> SourceConsumers {
        self.consumers.borrow_and_update().clone()
    }

    /// Resolves once the recorders changed since consumers() last returned.
    pub async fn changed(&mut self) {
        // The thread only returns after close() drops the receiver, so an
        // error here means it panicked. Never resolve then, or a select! on
        // this would spin.
        if self.consumers.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }

    /// Stop the thread and wait for it on the blocking pool.
    pub async fn close(self) {
        let Self { consumers, thread } = self;
        // The thread stops once no receiver is left.
        drop(consumers);
        let _ = tokio::task::spawn_blocking(move || thread.join()).await;
    }
}

/// A live connection that reports who records from one source.
trait Recorders {
    /// Ask the server who records now.
    fn read(&mut self) -> Result<SourceConsumers, SoundServerError>;
    /// Wait up to WAIT_STEP for the server. True when a recorder may have
    /// come, left, paused or resumed since the last call; an error means the
    /// connection is gone.
    fn wait_for_change(&mut self) -> Result<bool, SoundServerError>;
}

/// Keep `tx` current until its receiver goes away, reconnecting with backoff
/// whenever the connection cannot be opened or drops.
fn watch_recorders<R: Recorders>(
    tx: &watch::Sender<SourceConsumers>,
    mut connect: impl FnMut() -> Result<R, SoundServerError>,
    mut pause: impl FnMut(Duration),
) {
    let mut failures: u32 = 0;
    while !tx.is_closed() {
        let lost = connect().and_then(|mut recorders| {
            failures = 0;
            follow(&mut recorders, tx)
        });
        if tx.is_closed() {
            return;
        }
        if let Err(e) = lost {
            let delay = reconnect_delay(failures);
            failures = failures.saturating_add(1);
            warn!(
                "[hires] cannot watch for recorders ({}), reconnecting in {}s",
                e,
                delay.as_secs()
            );
            pause(delay);
        }
    }
}

fn reconnect_delay(failures: u32) -> Duration {
    RECONNECT_BASE_DELAY
        .saturating_mul(1 << failures.min(16))
        .min(RECONNECT_MAX_DELAY)
}

/// Publish the recorders now and after every change, until the receiver goes
/// away (Ok) or the connection drops (Err).
fn follow(
    recorders: &mut impl Recorders,
    tx: &watch::Sender<SourceConsumers>,
) -> Result<(), SoundServerError> {
    // Changes made while there was no connection were never reported, so
    // every connection starts with a read.
    let mut stale = true;
    while !tx.is_closed() {
        if stale {
            match recorders.read() {
                Ok(seen) => {
                    stale = false;
                    tx.send_if_modified(|current| {
                        let changed = *current != seen;
                        *current = seen;
                        changed
                    });
                },
                // Keep the last answer and read again after the next wait.
                Err(e) => debug!("[hires] could not check for recorders: {}", e),
            }
        }
        stale |= recorders.wait_for_change()?;
    }
    Ok(())
}

/// Sleep for `delay`, or less once the receiver went away.
fn pause_unless_closed(tx: &watch::Sender<SourceConsumers>, delay: Duration) {
    let until = Instant::now() + delay;
    while !tx.is_closed() {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left.min(Duration::from(WAIT_STEP)));
    }
}

/// The system sound server, subscribed to source output events.
struct PulseRecorders {
    source: &'static str,
    // Declared before the mainloop so the connection closes while its
    // mainloop still exists.
    context: Context,
    mainloop: Mainloop,
    // Set by the subscription callback, cleared by wait_for_change().
    changed: Rc<Cell<bool>>,
}

impl PulseRecorders {
    fn connect(
        source: &'static str,
        cancelled: impl Fn() -> bool,
    ) -> Result<PulseRecorders, SoundServerError> {
        let (mut mainloop, mut context) = connect_cancellable(cancelled)?;
        let changed = Rc::new(Cell::new(false));
        context.set_subscribe_callback(Some(Box::new({
            let changed = changed.clone();
            move |facility, _, _| {
                if facility == Some(Facility::SourceOutput) {
                    changed.set(true);
                }
            }
        })));
        let subscribed = Rc::new(Cell::new(false));
        let mut op = context.subscribe(InterestMaskSet::SOURCE_OUTPUT, {
            let subscribed = subscribed.clone();
            move |success| subscribed.set(success)
        });
        wait_for(&mut mainloop, &mut op)?;
        if !subscribed.get() {
            return Err(SoundServerError::SubscribeFailed);
        }
        Ok(PulseRecorders {
            source,
            context,
            mainloop,
            changed,
        })
    }
}

impl Recorders for PulseRecorders {
    fn read(&mut self) -> Result<SourceConsumers, SoundServerError> {
        source_consumers_on(&mut self.mainloop, &self.context, self.source)
    }

    fn wait_for_change(&mut self) -> Result<bool, SoundServerError> {
        let lost = |_| SoundServerError::ConnectionLost;
        self.mainloop.prepare(Some(WAIT_STEP)).map_err(lost)?;
        self.mainloop.poll().map_err(lost)?;
        self.mainloop.dispatch().map_err(lost)?;
        if !matches!(self.context.get_state(), ContextState::Ready) {
            return Err(SoundServerError::ConnectionLost);
        }
        Ok(self.changed.replace(false))
    }
}

#[cfg(test)]
mod tests {
    use {super::*, std::collections::VecDeque};

    fn running(app: &str) -> SourceConsumers {
        SourceConsumers {
            active: Some(app.to_string()),
            present: true,
        }
    }

    /// What the fake server does on one wait.
    enum Step {
        Quiet,
        Change(SourceConsumers),
        FailNextRead,
        Drop,
    }

    struct FakeRecorders {
        now: SourceConsumers,
        steps: VecDeque<Step>,
        fail_read: bool,
        reads: Rc<Cell<u32>>,
    }

    impl FakeRecorders {
        fn new(
            now: SourceConsumers,
            steps: impl IntoIterator<Item = Step>,
            reads: &Rc<Cell<u32>>,
        ) -> Self {
            FakeRecorders {
                now,
                steps: steps.into_iter().collect(),
                fail_read: false,
                reads: reads.clone(),
            }
        }
    }

    impl Recorders for FakeRecorders {
        fn read(&mut self) -> Result<SourceConsumers, SoundServerError> {
            self.reads.set(self.reads.get() + 1);
            if std::mem::take(&mut self.fail_read) {
                return Err(SoundServerError::OperationTimeout);
            }
            Ok(self.now.clone())
        }

        fn wait_for_change(&mut self) -> Result<bool, SoundServerError> {
            match self.steps.pop_front() {
                Some(Step::Quiet) => Ok(false),
                Some(Step::Change(now)) => {
                    self.now = now;
                    Ok(true)
                },
                Some(Step::FailNextRead) => {
                    self.fail_read = true;
                    Ok(true)
                },
                Some(Step::Drop) | None => Err(SoundServerError::ConnectionLost),
            }
        }
    }

    /// Run the watch over scripted connection attempts. Every attempt ends in
    /// an error, so the watch pauses once per attempt; the receiver goes away
    /// at the last pause. Returns the last published recorders and the pauses.
    fn run(
        attempts: Vec<Result<FakeRecorders, SoundServerError>>,
    ) -> (SourceConsumers, Vec<Duration>) {
        let (tx, rx) = watch::channel(SourceConsumers::default());
        let total = attempts.len();
        let mut attempts = VecDeque::from(attempts);
        let mut rx = Some(rx);
        let mut pauses = Vec::new();
        watch_recorders(
            &tx,
            || attempts.pop_front().expect("no connection attempt left"),
            |delay| {
                pauses.push(delay);
                if pauses.len() == total {
                    rx = None;
                }
            },
        );
        let last = tx.borrow().clone();
        (last, pauses)
    }

    #[test]
    fn recorders_are_read_again_only_after_a_change() {
        let reads = Rc::new(Cell::new(0));
        let steps = [
            Step::Quiet,
            Step::Quiet,
            Step::Change(running("Discord")),
            Step::Quiet,
            Step::Drop,
        ];
        let (last, pauses) = run(vec![Ok(FakeRecorders::new(
            SourceConsumers::default(),
            steps,
            &reads,
        ))]);

        assert_eq!(last, running("Discord"));
        // One read on connect and one after the change, on a single connection.
        assert_eq!(reads.get(), 2);
        assert_eq!(pauses.len(), 1);
    }

    #[test]
    fn a_failed_read_keeps_the_last_answer_and_reads_again() {
        let reads = Rc::new(Cell::new(0));
        let steps = [Step::FailNextRead, Step::Quiet, Step::Drop];
        let (last, _) = run(vec![Ok(FakeRecorders::new(
            running("Discord"),
            steps,
            &reads,
        ))]);

        assert_eq!(last, running("Discord"));
        assert_eq!(reads.get(), 3);
    }

    #[test]
    fn a_dropped_connection_reconnects_with_backoff_and_reads_again() {
        let reads = Rc::new(Cell::new(0));
        let paused = SourceConsumers {
            active: None,
            present: true,
        };
        let (last, delays) = run(vec![
            Ok(FakeRecorders::new(running("Discord"), [Step::Drop], &reads)),
            Err(SoundServerError::ConnectTimeout),
            Err(SoundServerError::ConnectFailed),
            Ok(FakeRecorders::new(paused.clone(), [Step::Drop], &reads)),
        ]);

        assert_eq!(last, paused);
        // A connection that opened starts the backoff over.
        let secs: Vec<u64> = delays.iter().map(Duration::as_secs).collect();
        assert_eq!(secs, [1, 2, 4, 1]);
    }

    #[test]
    fn reconnect_delay_doubles_up_to_the_cap() {
        assert_eq!(reconnect_delay(0), RECONNECT_BASE_DELAY);
        assert_eq!(reconnect_delay(1), RECONNECT_BASE_DELAY * 2);
        assert_eq!(reconnect_delay(4), RECONNECT_BASE_DELAY * 16);
        assert_eq!(reconnect_delay(5), RECONNECT_MAX_DELAY);
        assert_eq!(reconnect_delay(u32::MAX), RECONNECT_MAX_DELAY);
    }
}
