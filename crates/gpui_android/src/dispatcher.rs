use std::{mem::MaybeUninit, thread, time::Duration};

use android_activity::{AndroidApp, AndroidAppWaker};
use calloop::{EventLoop, channel::Sender, timer::TimeoutAction};
use util::ResultExt;

use gpui::{
    PlatformDispatcher, Priority, PriorityQueueReceiver, PriorityQueueSender, RunnableVariant,
    profiler,
};

const MIN_THREADS: usize = 2;

struct TimerAfter {
    duration: Duration,
    runnable: RunnableVariant,
}

pub(crate) struct AndroidDispatcher {
    main_sender: PriorityQueueAndroidSender<RunnableVariant>,
    timer_sender: Sender<TimerAfter>,
    background_sender: PriorityQueueSender<RunnableVariant>,
    _background_threads: Vec<thread::JoinHandle<()>>,
    main_thread_id: thread::ThreadId,
}

impl AndroidDispatcher {
    /// Construct on the main thread. Returns the dispatcher plus a receiver that
    /// the platform's run loop drains each tick to execute main-thread runnables.
    pub fn new(
        android_app: &AndroidApp,
    ) -> (Self, PriorityQueueReceiver<RunnableVariant>) {
        let main_waker = android_app.create_waker();
        let (main_sender_inner, main_receiver) = PriorityQueueReceiver::new();
        let main_sender = PriorityQueueAndroidSender::new(main_sender_inner, main_waker);

        let (background_sender, background_receiver) = PriorityQueueReceiver::new();
        let thread_count =
            std::thread::available_parallelism().map_or(MIN_THREADS, |i| i.get().max(MIN_THREADS));

        let mut background_threads = (0..thread_count)
            .map(|i| {
                let receiver: PriorityQueueReceiver<RunnableVariant> = background_receiver.clone();
                std::thread::Builder::new()
                    .name(format!("Worker-{i}"))
                    .spawn(move || {
                        for runnable in receiver.iter() {
                            let location = runnable.metadata().location;
                            let spawned = runnable.metadata().spawned;
                            profiler::update_running_task(spawned, location);
                            runnable.run();
                            profiler::save_task_timing();
                        }
                    })
                    .unwrap()
            })
            .collect::<Vec<_>>();

        let (timer_sender, timer_channel) = calloop::channel::channel::<TimerAfter>();
        let timer_thread = std::thread::Builder::new()
            .name("Timer".to_owned())
            .spawn(move || {
                let mut event_loop: EventLoop<()> =
                    EventLoop::try_new().expect("failed to initialize timer loop");

                let handle = event_loop.handle();
                let timer_handle = event_loop.handle();
                handle
                    .insert_source(timer_channel, move |e, _, _| {
                        if let calloop::channel::Event::Msg(timer) = e {
                            let mut runnable = Some(timer.runnable);
                            timer_handle
                                .insert_source(
                                    calloop::timer::Timer::from_duration(timer.duration),
                                    move |_, _, _| {
                                        if let Some(runnable) = runnable.take() {
                                            let location = runnable.metadata().location;
                                            let spawned = runnable.metadata().spawned;
                                            profiler::update_running_task(spawned, location);
                                            runnable.run();
                                            profiler::save_task_timing();
                                        }
                                        TimeoutAction::Drop
                                    },
                                )
                                .expect("failed to start timer");
                        }
                    })
                    .expect("failed to start timer thread");

                event_loop.run(None, &mut (), |_| {}).log_err();
            })
            .unwrap();

        background_threads.push(timer_thread);

        let dispatcher = Self {
            main_sender,
            timer_sender,
            background_sender,
            _background_threads: background_threads,
            main_thread_id: thread::current().id(),
        };

        (dispatcher, main_receiver)
    }
}

impl PlatformDispatcher for AndroidDispatcher {
    fn is_main_thread(&self) -> bool {
        thread::current().id() == self.main_thread_id
    }

    fn dispatch(&self, runnable: RunnableVariant, priority: Priority) {
        self.background_sender
            .send(priority, runnable)
            .unwrap_or_else(|_| panic!("background sender returned without value"));
    }

    fn dispatch_on_main_thread(&self, runnable: RunnableVariant, priority: Priority) {
        self.main_sender
            .send(priority, runnable)
            .unwrap_or_else(|runnable| {
                // The Runnable may wrap a !Send Future; the main receiver dropping means we
                // are shutting down and on a background thread. Forget rather than drop here
                // to avoid undefined behavior from dropping !Send on the wrong thread.
                std::mem::forget(runnable);
            });
    }

    fn dispatch_after(&self, duration: Duration, runnable: RunnableVariant) {
        self.timer_sender
            .send(TimerAfter { duration, runnable })
            .ok();
    }

    fn spawn_realtime(&self, f: Box<dyn FnOnce() + Send>) {
        std::thread::spawn(move || {
            // SAFETY: pthread_self is always safe.
            let thread_id = unsafe { libc::pthread_self() };

            let policy = libc::SCHED_FIFO;
            let sched_priority = 65;

            // SAFETY: sched_param is valid when zero-initialized.
            let mut sched_param =
                unsafe { MaybeUninit::<libc::sched_param>::zeroed().assume_init() };
            sched_param.sched_priority = sched_priority;
            // SAFETY: sched_param is initialized above.
            let result = unsafe { libc::pthread_setschedparam(thread_id, policy, &sched_param) };
            if result != 0 {
                log::warn!("failed to set realtime thread priority");
            }

            f();
        });
    }
}

/// Wraps a `PriorityQueueSender` with an `AndroidAppWaker` that wakes the
/// android-activity main loop after each enqueue. Equivalent to Linux's
/// `PriorityQueueCalloopSender` (which wakes via `calloop::ping`).
pub struct PriorityQueueAndroidSender<T> {
    sender: PriorityQueueSender<T>,
    waker: AndroidAppWaker,
}

impl<T> PriorityQueueAndroidSender<T> {
    fn new(sender: PriorityQueueSender<T>, waker: AndroidAppWaker) -> Self {
        Self { sender, waker }
    }

    pub fn send(&self, priority: Priority, item: T) -> Result<(), gpui::queue::SendError<T>> {
        let res = self.sender.send(priority, item);
        if res.is_ok() {
            self.waker.wake();
        }
        res
    }
}

impl<T> Drop for PriorityQueueAndroidSender<T> {
    fn drop(&mut self) {
        self.waker.wake();
    }
}
