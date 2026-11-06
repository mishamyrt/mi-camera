use crate::{
    capture::{Capture, Frame},
    hevc,
};
use std::{
    io,
    sync::{
        mpsc::{self, Receiver, SyncSender},
        Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Default)]
struct State {
    parameters: [Option<Vec<u8>>; 3],
    subscribers: Vec<SyncSender<Arc<Frame>>>,
    last_frame: Option<Instant>,
}

/// A shared capture worker that caches HEVC parameters and disconnects slow readers.
#[derive(Default)]
pub struct Hub {
    state: Mutex<State>,
    changed: Condvar,
}

impl Hub {
    /// Start a capture worker that reconnects to the socket after errors.
    pub fn start(path: &'static str) -> io::Result<Arc<Self>> {
        let hub = Arc::new(Self::default());
        let worker = hub.clone();
        thread::Builder::new()
            .name(path.into())
            .stack_size(128 * 1024)
            .spawn(move || loop {
                let result = (|| -> io::Result<()> {
                    let mut capture = Capture::connect(path)?;
                    loop {
                        let frame = Arc::new(capture.next_frame()?);
                        let mut state = worker.state.lock().unwrap();
                        for nal in hevc::nals(&frame.data) {
                            if let kind @ 32..=34 = hevc::nal_type(nal) {
                                state.parameters[(kind - 32) as usize] = Some(nal.to_vec());
                            }
                        }
                        state.last_frame = Some(Instant::now());
                        // Disconnect slow readers rather than grow queues or send broken GOPs.
                        state
                            .subscribers
                            .retain(|tx| tx.try_send(frame.clone()).is_ok());
                        worker.changed.notify_all();
                    }
                })();
                eprintln!("{path}: {} (retrying)", result.unwrap_err());
                {
                    let mut state = worker.state.lock().unwrap();
                    state.parameters = Default::default();
                    state.last_frame = None;
                    state.subscribers.clear();
                }
                thread::sleep(Duration::from_secs(1));
            })?;
        Ok(hub)
    }

    /// Wait up to ten seconds for fresh VPS, SPS, and PPS NAL units, in that order.
    pub fn parameters(&self) -> io::Result<[Vec<u8>; 3]> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut state = self.state.lock().unwrap();
        loop {
            if state.parameters.iter().all(Option::is_some)
                && state
                    .last_frame
                    .is_some_and(|t| t.elapsed() < Duration::from_secs(2))
            {
                return Ok(state.parameters.clone().map(Option::unwrap));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "camera stream unavailable",
                ));
            }
            state = self.changed.wait_timeout(state, remaining).unwrap().0;
        }
    }

    /// Subscribe with a two-frame queue; a full queue disconnects the subscriber.
    pub fn subscribe(&self) -> Receiver<Arc<Frame>> {
        let (tx, rx) = mpsc::sync_channel(2);
        self.state.lock().unwrap().subscribers.push(tx);
        rx
    }
}
