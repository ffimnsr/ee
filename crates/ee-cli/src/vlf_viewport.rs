use std::collections::HashMap;
use std::io;

use serde_json::json;
use tokio::sync::mpsc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VlfViewportRequest {
    view_id: String,
    line_start: u64,
    line_end: u64,
    generation: u64,
}

impl VlfViewportRequest {
    pub(crate) fn new(view_id: String, line_start: u64, line_end: u64, generation: u64) -> Self {
        Self { view_id, line_start, line_end, generation }
    }
}

#[derive(Debug)]
struct ScheduledView {
    active_generation: Option<u64>,
    orphaned_generation: Option<u64>,
    queued: Option<VlfViewportRequest>,
}

impl ScheduledView {
    fn is_idle(&self) -> bool {
        self.active_generation.is_none()
            && self.orphaned_generation.is_none()
            && self.queued.is_none()
    }
}

/// Keeps one active VLF viewport request and one latest queued request per view.
///
/// Cancelling an already-sent tail request cannot stop xi-core. Scheduler permits one replacement
/// request beside that orphan, but later positions still coalesce until both responses arrive.
#[derive(Debug, Default)]
pub(crate) struct VlfViewportScheduler {
    scheduled: HashMap<String, ScheduledView>,
}

impl VlfViewportScheduler {
    pub(crate) fn submit(
        &mut self,
        tx: &mpsc::Sender<String>,
        request: VlfViewportRequest,
    ) -> io::Result<()> {
        let view_id = request.view_id.clone();
        let generation = request.generation;
        if let Some(scheduled) = self.scheduled.get_mut(&view_id) {
            if scheduled.active_generation.is_some() {
                scheduled.queued = Some(request);
                return Ok(());
            }
            scheduled.active_generation = Some(generation);
            scheduled.queued = None;
        } else {
            self.scheduled.insert(
                view_id.clone(),
                ScheduledView {
                    active_generation: Some(generation),
                    orphaned_generation: None,
                    queued: None,
                },
            );
        }

        if let Err(error) = send_request(tx, &request) {
            self.clear_failed_send(&view_id, generation);
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn response_received(
        &mut self,
        tx: &mpsc::Sender<String>,
        view_id: &str,
        generation: u64,
    ) -> io::Result<()> {
        let Some(scheduled) = self.scheduled.get_mut(view_id) else {
            return Ok(());
        };
        if scheduled.orphaned_generation == Some(generation) {
            scheduled.orphaned_generation = None;
        } else if scheduled.active_generation == Some(generation) {
            scheduled.active_generation = None;
        } else {
            return Ok(());
        }
        if scheduled.orphaned_generation.is_some() || scheduled.active_generation.is_some() {
            return Ok(());
        }

        let Some(next) = scheduled.queued.take() else {
            self.scheduled.remove(view_id);
            return Ok(());
        };
        let next_generation = next.generation;
        scheduled.active_generation = Some(next_generation);
        if let Err(error) = send_request(tx, &next) {
            self.clear_failed_send(view_id, next_generation);
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn cancel_request(&mut self, view_id: &str, generation: u64) {
        let Some(scheduled) = self.scheduled.get_mut(view_id) else {
            return;
        };
        if scheduled.queued.as_ref().is_some_and(|request| request.generation == generation) {
            scheduled.queued = None;
        }
        if scheduled.active_generation == Some(generation)
            && scheduled.orphaned_generation.is_none()
        {
            scheduled.active_generation = None;
            scheduled.orphaned_generation = Some(generation);
        }
        if scheduled.is_idle() {
            self.scheduled.remove(view_id);
        }
    }

    pub(crate) fn cancel_view(&mut self, view_id: &str) {
        self.scheduled.remove(view_id);
    }

    fn clear_failed_send(&mut self, view_id: &str, generation: u64) {
        let Some(scheduled) = self.scheduled.get_mut(view_id) else {
            return;
        };
        if scheduled.active_generation == Some(generation) {
            scheduled.active_generation = None;
        }
        if scheduled.is_idle() {
            self.scheduled.remove(view_id);
        }
    }
}

fn send_request(tx: &mpsc::Sender<String>, request: &VlfViewportRequest) -> io::Result<()> {
    let raw = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "method": "edit",
        "params": {
            "view_id": request.view_id,
            "method": "vlf_viewport",
            "params": {
                "line_start": request.line_start,
                "line_end": request.line_end,
                "generation": request.generation,
            },
        },
    }))
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    tx.blocking_send(raw)
        .map_err(|error| io::Error::new(io::ErrorKind::BrokenPipe, error.to_string()))
}
