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

/// Longest the watch thread waits for a sound server event (200 ms) before it
/// checks whether anyone still listens. close() can wait longer: a read or
/// subscribe already sent finishes first, and each of its requests may take up
/// to the sound server timeout (PULSE_TIMEOUT).
const WAIT_STEP: MicroSeconds = MicroSeconds(200_000);
/// The recorders are read again at least this often even when no event came,
/// so a missed source output event cannot keep a stale answer for good.
const RESYNC_INTERVAL: Duration = Duration::from_secs(5);
/// A connection that stayed up this long was healthy, so its loss starts the
/// reconnect backoff over. One that drops sooner, such as a server that accepts
/// and then closes, keeps backing off.
const STABLE_CONNECTION: Duration = Duration::from_secs(30);
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
                    Instant::now,
                );
            })?;
        Ok(SourceWatch { consumers, thread })
    }

    /// The latest recorders. A failed read keeps the last answer: one missed
    /// reading must not look like the recorder leaving. A lost connection
    /// reports no recorder until it is back.
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
    now: impl Fn() -> Instant,
) {
    let mut failures: u32 = 0;
    while !tx.is_closed() {
        let lost = connect().and_then(|mut recorders| {
            let opened = now();
            let lost = follow(&mut recorders, tx, &now);
            if now().duration_since(opened) >= STABLE_CONNECTION {
                failures = 0;
            }
            lost
        });
        if tx.is_closed() {
            return;
        }
        if let Err(e) = lost {
            // Without a connection nobody can tell whether the recorder is
            // still there. Report none, so a capture stops after its grace
            // period instead of running on a stale answer through the backoff.
            publish(tx, SourceConsumers::default());
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
    now: &impl Fn() -> Instant,
) -> Result<(), SoundServerError> {
    // Changes made while there was no connection were never reported, so
    // every connection starts with a read.
    let mut stale = true;
    let mut read_at = now();
    while !tx.is_closed() {
        stale |= now().duration_since(read_at) >= RESYNC_INTERVAL;
        if stale {
            read_at = now();
            match recorders.read() {
                Ok(seen) => {
                    stale = false;
                    publish(tx, seen);
                },
                // Keep the last answer and read again after the next wait.
                Err(e) => debug!("[hires] could not check for recorders: {}", e),
            }
        }
        stale |= recorders.wait_for_change()?;
    }
    Ok(())
}

/// Replace the published recorders, waking the receiver only when they changed.
fn publish(tx: &watch::Sender<SourceConsumers>, seen: SourceConsumers) {
    tx.send_if_modified(|current| {
        let changed = *current != seen;
        *current = seen;
        changed
    });
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
        // read() runs the mainloop too, so an event may already be in.
        if self.changed.replace(false) {
            return Ok(true);
        }
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

    /// How many waits of WAIT_STEP fit in `span`.
    fn waits_in(span: Duration) -> usize {
        let step = Duration::from(WAIT_STEP).as_micros();
        usize::try_from(span.as_micros() / step).unwrap()
    }

    /// What the fake server does on one wait.
    enum Step {
        Quiet,
        Change(SourceConsumers),
        /// The recorders change but the event never arrives.
        Missed(SourceConsumers),
        FailNextRead,
        Drop,
    }

    /// A fake sound server: counts reads over all its connections, and moves
    /// a fake clock by WAIT_STEP on every wait.
    #[derive(Clone)]
    struct Server {
        reads: Rc<Cell<u32>>,
        clock: Rc<Cell<Instant>>,
    }

    impl Server {
        fn new() -> Self {
            Server {
                reads: Rc::default(),
                clock: Rc::new(Cell::new(Instant::now())),
            }
        }

        fn now(&self) -> Instant {
            self.clock.get()
        }

        fn advance(&self, by: Duration) {
            self.clock.set(self.clock.get() + by);
        }

        fn reads(&self) -> u32 {
            self.reads.get()
        }

        fn connection(
            &self,
            now: SourceConsumers,
            steps: impl IntoIterator<Item = Step>,
        ) -> FakeRecorders {
            FakeRecorders {
                now,
                steps: steps.into_iter().collect(),
                fail_read: false,
                server: self.clone(),
            }
        }
    }

    struct FakeRecorders {
        now: SourceConsumers,
        steps: VecDeque<Step>,
        fail_read: bool,
        server: Server,
    }

    impl Recorders for FakeRecorders {
        fn read(&mut self) -> Result<SourceConsumers, SoundServerError> {
            self.server.reads.set(self.server.reads.get() + 1);
            if std::mem::take(&mut self.fail_read) {
                return Err(SoundServerError::OperationTimeout);
            }
            Ok(self.now.clone())
        }

        fn wait_for_change(&mut self) -> Result<bool, SoundServerError> {
            self.server.advance(Duration::from(WAIT_STEP));
            match self.steps.pop_front() {
                Some(Step::Quiet) => Ok(false),
                Some(Step::Change(now)) => {
                    self.now = now;
                    Ok(true)
                },
                Some(Step::Missed(now)) => {
                    self.now = now;
                    Ok(false)
                },
                Some(Step::FailNextRead) => {
                    self.fail_read = true;
                    Ok(true)
                },
                Some(Step::Drop) | None => Err(SoundServerError::ConnectionLost),
            }
        }
    }

    /// Follow one connection until it drops. Returns what it published last.
    fn follow_until_dropped(server: &Server, mut recorders: FakeRecorders) -> SourceConsumers {
        let (tx, _rx) = watch::channel(SourceConsumers::default());
        let lost = follow(&mut recorders, &tx, &|| server.now());
        assert!(matches!(lost, Err(SoundServerError::ConnectionLost)));
        tx.borrow().clone()
    }

    /// Run the watch over scripted connection attempts. Every attempt ends in
    /// an error, so the watch pauses once per attempt; the receiver goes away
    /// at the last pause. Returns each pause with what was published when it
    /// began.
    fn run(
        server: &Server,
        attempts: Vec<Result<FakeRecorders, SoundServerError>>,
    ) -> Vec<(Duration, SourceConsumers)> {
        let (tx, rx) = watch::channel(SourceConsumers::default());
        let total = attempts.len();
        let mut attempts = VecDeque::from(attempts);
        let mut rx = Some(rx);
        let mut pauses = Vec::new();
        watch_recorders(
            &tx,
            || attempts.pop_front().expect("no connection attempt left"),
            |delay| {
                pauses.push((delay, tx.borrow().clone()));
                server.advance(delay);
                if pauses.len() == total {
                    rx = None;
                }
            },
            || server.now(),
        );
        pauses
    }

    fn delays(pauses: &[(Duration, SourceConsumers)]) -> Vec<u64> {
        pauses.iter().map(|(delay, _)| delay.as_secs()).collect()
    }

    #[test]
    fn recorders_are_read_again_only_after_a_change() {
        let server = Server::new();
        let steps = [
            Step::Quiet,
            Step::Quiet,
            Step::Change(running("Discord")),
            Step::Quiet,
            Step::Drop,
        ];
        let last = follow_until_dropped(
            &server,
            server.connection(SourceConsumers::default(), steps),
        );

        assert_eq!(last, running("Discord"));
        // One read on connect and one after the change.
        assert_eq!(server.reads(), 2);
    }

    #[test]
    fn a_missed_event_is_caught_by_the_periodic_read() {
        let per_resync = waits_in(RESYNC_INTERVAL);
        let follow_for = |waits: usize| {
            let server = Server::new();
            let steps = std::iter::once(Step::Missed(running("Discord")))
                .chain(std::iter::repeat_with(|| Step::Quiet).take(waits - 1))
                .chain([Step::Drop]);
            let last = follow_until_dropped(
                &server,
                server.connection(SourceConsumers::default(), steps),
            );
            (last, server.reads())
        };

        // Short of the interval only the read on connect happened.
        assert_eq!(follow_for(per_resync - 1), (SourceConsumers::default(), 1));
        // Quiet waits past the interval cause one more read, which finds the
        // recorder the event never reported.
        assert_eq!(follow_for(per_resync), (running("Discord"), 2));
        assert_eq!(follow_for(2 * per_resync - 1), (running("Discord"), 2));
    }

    #[test]
    fn a_failed_read_keeps_the_last_answer_and_reads_again() {
        let server = Server::new();
        let steps = [Step::FailNextRead, Step::Quiet, Step::Drop];
        let last = follow_until_dropped(&server, server.connection(running("Discord"), steps));

        assert_eq!(last, running("Discord"));
        assert_eq!(server.reads(), 3);
    }

    #[test]
    fn a_lost_connection_reports_no_recorder_through_the_backoff() {
        let server = Server::new();
        let pauses = run(
            &server,
            vec![Ok(
                server.connection(running("Discord"), [Step::Quiet, Step::Drop])
            )],
        );

        assert_eq!(server.reads(), 1);
        assert_eq!(pauses, [(RECONNECT_BASE_DELAY, SourceConsumers::default())]);
    }

    #[test]
    fn short_lived_connections_keep_backing_off() {
        let server = Server::new();
        let pauses = run(
            &server,
            vec![
                Ok(server.connection(running("Discord"), [Step::Drop])),
                Err(SoundServerError::ConnectTimeout),
                Err(SoundServerError::ConnectFailed),
                Ok(server.connection(running("Discord"), [Step::Drop])),
            ],
        );

        // A server that accepts and drops at once does not start the backoff
        // over, and every connection starts with a read.
        assert_eq!(delays(&pauses), [1, 2, 4, 8]);
        assert_eq!(server.reads(), 2);
    }

    #[test]
    fn a_connection_that_stayed_up_starts_the_backoff_over() {
        let server = Server::new();
        let steps = std::iter::repeat_with(|| Step::Quiet)
            .take(waits_in(STABLE_CONNECTION))
            .chain([Step::Drop]);
        let pauses = run(
            &server,
            vec![
                Err(SoundServerError::ConnectTimeout),
                Err(SoundServerError::ConnectFailed),
                Ok(server.connection(running("Discord"), steps)),
            ],
        );

        assert_eq!(delays(&pauses), [1, 2, 1]);
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
