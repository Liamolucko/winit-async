use std::future::Future;
use std::mem;
use std::pin::Pin;
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
    let waker = create_waker(&event_loop);

    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::Wait;

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

        match future.poll(&mut Context::from_waker(&waker)) {
            Poll::Ready(()) => {
                *control_flow = ControlFlow::Exit;
                state = State::Done;
            }
            Poll::Pending => {}
        }
    });
}

fn create_waker(event_loop: &EventLoop<()>) -> Waker {
    struct ProxyWaker(Mutex<EventLoopProxy<()>>);

    impl Wake for ProxyWaker {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref()
        }

        fn wake_by_ref(self: &Arc<Self>) {
            match self.0.try_lock() {
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

    Arc::new(ProxyWaker(Mutex::new(event_loop.create_proxy()))).into()
}
