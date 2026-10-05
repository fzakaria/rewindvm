//! The app's blocking work: engine calls, which wait on a `rewind` child
//! process for seconds, a minute for a fork, or longer while gdb fetches
//! debug info. GPUI's background executor has a few threads, about one per
//! CPU, for work that computes; a call that only waits would hold one of
//! them the whole time, and a handful at once would leave none for opening
//! a run or reading the runs list. So each runs on a thread of its own.

use std::future::Future;

/// Runs `call` on a thread of its own, and resolves to what it returns.
pub fn on_own_thread<T: Send + 'static>(
    call: impl FnOnce() -> T + Send + 'static,
) -> impl Future<Output = T> {
    let (answer, answered) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let _ = answer.send(call());
    });
    async move {
        answered
            .await
            .expect("a blocking call's thread ended without answering")
    }
}

#[cfg(test)]
mod tests {
    // Blocking calls run on threads of their own: several that wait at
    // once all finish, without an executor.
    use super::*;

    #[test]
    fn calls_that_wait_at_once_all_answer() {
        // Eight calls that each wait on a barrier for all eight: on a pool
        // smaller than eight threads they could never all start.
        const CALLS: usize = 8;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(CALLS));
        let answers: Vec<_> = (0..CALLS)
            .map(|i| {
                let barrier = barrier.clone();
                on_own_thread(move || {
                    barrier.wait();
                    i
                })
            })
            .collect();
        let got: Vec<usize> = futures::executor::block_on(futures::future::join_all(answers));
        assert_eq!(got, (0..CALLS).collect::<Vec<_>>());
    }
}
