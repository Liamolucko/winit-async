use std::future::Future;
use std::mem;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::task::{Context, Poll, Wake, Waker};

use async_channel::{Receiver, Sender, TrySendError};
use futures_core::Stream;
use winit::event::Event;
use winit::event_loop::{ControlFlow, EventLoop, EventLoopProxy, EventLoopWindowTarget};

#[derive(Debug)]
pub struct Events(Receiver<Event<'static, ()>>);

impl Stream for Events {
    type Item = Event<'static, ()>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.0).poll_next(cx)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

// TODO: support user events (implementation isn't hard, API is just annoying).
pub fn run<F, Fut>(event_loop: EventLoop<()>, callback: F)
where
    F: 'static + FnOnce(&'static EventLoopWindowTarget<()>, Events) -> Fut,
    Fut: Future<Output = ()> + 'static,
{
    enum State<F, Fut> {
        Init(F),
        Running(Fut, Sender<Event<'static, ()>>),
        Done,
    }

    let mut state = State::Init(callback);

    // A boolean shared between here and the wakers which is set to `true` if we're
    // about to poll the future anyway, in which case the wakers do nothing.
    let will_poll = Arc::new(AtomicBool::new(false));

    let waker = create_waker(&event_loop, will_poll.clone());

    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::Wait;
        will_poll.store(true, Ordering::Relaxed);

        if matches!(state, State::Init(_)) {
            let callback = match mem::replace(&mut state, State::Done) {
                State::Init(callback) => callback,
                _ => unreachable!(),
            };
            let (tx, rx) = async_channel::unbounded();
            state = State::Running(Box::pin(callback(target, Events(rx))), tx);
        }

        let (future, tx) = match &mut state {
            State::Init(_) => unreachable!(),
            State::Running(future, tx) => (future.as_mut(), tx),
            State::Done => return,
        };

        if event != Event::UserEvent(()) {
            // TODO: end the stream on `LoopDestroyed`

            // TODO: define our own `'static` event type which doesn't have the
            // instant-resizing feature of `ScaleFactorChanged`
            match tx.try_send(event.to_static().unwrap()) {
                Ok(_) => {}
                // We don't care if they've stopped listening for events, just ignore it.
                Err(TrySendError::Closed(_)) => {}
                // This should be impossible.
                Err(TrySendError::Full(_)) => unreachable!("channel is unbounded"),
            }
        }

        // Set this to false right before polling the future because we want any wakes
        // inside of the poll to go through.
        will_poll.store(false, Ordering::Relaxed);

        match future.poll(&mut Context::from_waker(&waker)) {
            Poll::Ready(()) => {
                *control_flow = ControlFlow::Exit;
                state = State::Done;
            }
            Poll::Pending => {}
        }
    });
}

fn create_waker(event_loop: &EventLoop<()>, will_poll: Arc<AtomicBool>) -> Waker {
    struct ProxyWaker {
        proxy: Mutex<EventLoopProxy<()>>,
        will_poll: Arc<AtomicBool>,
    }

    impl Wake for ProxyWaker {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref()
        }

        fn wake_by_ref(self: &Arc<Self>) {
            if self.will_poll.load(Ordering::Relaxed) {
                // The event loop is already running, no need to poll it.
                return;
            }

            match self.proxy.try_lock() {
                Ok(proxy) => {
                    // Note: this only returns an error if the event loop is closed, in which case
                    // we don't have to do anything anyway because there's nothing to wake.
                    let _ = proxy.send_event(());
                }
                // If it's already locked just return, since the other holder of the lock is going
                // to wake the event loop anyway.
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Poisoned(e)) => panic!("mutex poisoned: {e}"),
            }
        }
    }

    Arc::new(ProxyWaker {
        proxy: Mutex::new(event_loop.create_proxy()),
        will_poll,
    })
    .into()
}
