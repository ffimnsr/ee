//! WebSocket transport + render scheduling for the WASM client.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use web_sys::{MessageEvent, WebSocket};

use crate::model::AppModel;

use super::ui;

/// Reconnect backoff cap.
const MAX_RECONNECT_MILLIS: u32 = 10_000;

/// Handlers rebuilt on every render and kept alive until the next render.
pub(crate) type HandlerList = Rc<RefCell<Vec<Closure<dyn FnMut()>>>>;

/// Shared runtime handed to DOM event handlers.
#[derive(Clone)]
pub(crate) struct Runtime {
    pub(crate) model: Rc<RefCell<AppModel>>,
    pub(crate) elements: ui::Elements,
    socket: Rc<RefCell<Option<WebSocket>>>,
    token: Rc<RefCell<Option<String>>>,
    user_closed: Rc<Cell<bool>>,
    render_scheduled: Rc<Cell<bool>>,
    reconnect_attempt: Rc<Cell<u32>>,
    /// Handlers rebuilt on every render; kept alive until the next render.
    pub(crate) render_handlers: HandlerList,
}

impl Runtime {
    pub(crate) fn new(model: Rc<RefCell<AppModel>>, elements: ui::Elements) -> Self {
        Self {
            model,
            elements,
            socket: Rc::new(RefCell::new(None)),
            token: Rc::new(RefCell::new(None)),
            user_closed: Rc::new(Cell::new(false)),
            render_scheduled: Rc::new(Cell::new(false)),
            reconnect_attempt: Rc::new(Cell::new(0)),
            render_handlers: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// Connects (or reconnects) with a bearer token.
    pub(crate) fn connect(&self, token: String) {
        let token = token.trim().to_string();
        if token.is_empty() {
            self.set_notice("enter the bearer token printed for this server");
            return;
        }
        if let Some(storage) = ui::session_storage() {
            let _ = storage.set_item("ee-web-token", &token);
        }
        self.user_closed.set(false);
        *self.token.borrow_mut() = Some(token.clone());
        self.open_socket(&token);
    }

    fn open_socket(&self, token: &str) {
        let Some(window) = web_sys::window() else {
            return;
        };
        let location = window.location();
        let scheme = if location.protocol().unwrap_or_default() == "https:" { "wss" } else { "ws" };
        let host = location.host().unwrap_or_default();
        let url = format!("{scheme}://{host}/ws?token={token}");
        let Ok(socket) = WebSocket::new(&url) else {
            self.set_notice("cannot open websocket");
            return;
        };
        socket.set_binary_type(web_sys::BinaryType::Arraybuffer);
        {
            let runtime = self.clone();
            let onopen = Closure::<dyn FnMut()>::new(move || {
                runtime.reconnect_attempt.set(0);
                runtime.model.borrow_mut().on_open();
                runtime.flush_outbound();
                runtime.schedule_render();
            });
            socket.set_onopen(Some(onopen.as_ref().unchecked_ref()));
            onopen.forget();
        }
        {
            let runtime = self.clone();
            let onmessage = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
                let Some(text) = event.data().as_string() else {
                    return;
                };
                if let Ok(frame) = serde_json::from_str::<serde_json::Value>(&text) {
                    runtime.model.borrow_mut().apply_frame(&frame);
                    runtime.flush_outbound();
                    runtime.schedule_render();
                }
            });
            socket.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
            onmessage.forget();
        }
        {
            let runtime = self.clone();
            let onclose = Closure::<dyn FnMut()>::new(move || {
                runtime.model.borrow_mut().on_close("socket closed");
                runtime.schedule_render();
                runtime.schedule_reconnect();
            });
            socket.set_onclose(Some(onclose.as_ref().unchecked_ref()));
            onclose.forget();
        }
        *self.socket.borrow_mut() = Some(socket);
        self.schedule_render();
    }

    fn schedule_reconnect(&self) {
        if self.user_closed.get() || self.token.borrow().is_none() {
            return;
        }
        let attempt = self.reconnect_attempt.get().saturating_add(1);
        self.reconnect_attempt.set(attempt);
        let delay = 1_000_u32.saturating_mul(1 << attempt.min(4)).min(MAX_RECONNECT_MILLIS);
        let Some(window) = web_sys::window() else {
            return;
        };
        let runtime = self.clone();
        let callback = Closure::once_into_js(move || {
            if let Some(token) = runtime.token.borrow().clone() {
                runtime.open_socket(&token);
            }
        });
        let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
            callback.unchecked_ref(),
            delay as i32,
        );
    }

    /// Sends every queued frame.
    pub(crate) fn flush_outbound(&self) {
        let frames = self.model.borrow_mut().drain_outbound();
        if frames.is_empty() {
            return;
        }
        let Some(socket) = self.socket.borrow().clone() else {
            return;
        };
        for frame in frames {
            let text = serde_json::to_string(&frame).unwrap_or_default();
            if socket.send_with_str(&text).is_err() {
                break;
            }
        }
    }

    pub(crate) fn set_notice(&self, message: impl Into<String>) {
        self.model.borrow_mut().notice = Some(message.into());
        self.schedule_render();
    }

    /// Renders once per animation frame, batching notification bursts.
    pub(crate) fn schedule_render(&self) {
        if self.render_scheduled.replace(true) {
            return;
        }
        let Some(window) = web_sys::window() else {
            return;
        };
        let runtime = self.clone();
        let callback = Closure::once_into_js(move || {
            runtime.render_scheduled.set(false);
            ui::render(&runtime);
        });
        let _ = window.request_animation_frame(callback.unchecked_ref());
    }

    pub(crate) fn disconnect(&self) {
        self.user_closed.set(true);
        if let Some(socket) = self.socket.borrow_mut().take() {
            let _ = socket.close();
        }
        self.model.borrow_mut().on_close("closed by user");
        self.schedule_render();
    }
}
