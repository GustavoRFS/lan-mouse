use async_trait::async_trait;
use core::task::{Context, Poll};
use event_thread::EventThread;
use futures::Stream;
use std::pin::Pin;

use input_event::MonitorDescriptor;
use std::task::ready;
use tokio::sync::mpsc::{Receiver, channel};

use super::{Capture, CaptureError, CaptureEvent, CapturePoint};

mod display_util;
mod event_thread;

pub struct WindowsInputCapture {
    event_rx: Receiver<(CapturePoint, CaptureEvent)>,
    event_thread: EventThread,
}

#[async_trait]
impl Capture for WindowsInputCapture {
    async fn create(&mut self, point: CapturePoint) -> Result<(), CaptureError> {
        self.event_thread.create(point)?;
        Ok(())
    }

    async fn destroy(&mut self, point: CapturePoint) -> Result<(), CaptureError> {
        self.event_thread.destroy(point)?;
        Ok(())
    }

    fn monitor_catalog(&self) -> Vec<MonitorDescriptor> {
        event_thread::enumerate_displays()
            .iter()
            .filter_map(|display| display.monitor_descriptor())
            .collect()
    }

    fn supports_monitor_grid(&self) -> bool {
        true
    }

    async fn release(&mut self) -> Result<(), CaptureError> {
        self.event_thread.release_capture()?;
        Ok(())
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }
}

impl WindowsInputCapture {
    pub(crate) fn new() -> Self {
        let (event_tx, event_rx) = channel(10);
        let event_thread = EventThread::new(event_tx);
        Self {
            event_thread,
            event_rx,
        }
    }
}

impl Stream for WindowsInputCapture {
    type Item = Result<(CapturePoint, CaptureEvent), CaptureError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match ready!(self.event_rx.poll_recv(cx)) {
            None => Poll::Ready(None),
            Some(e) => Poll::Ready(Some(Ok(e))),
        }
    }
}
