//! DOM layer for the web client.
//!
//! Everything is built from text nodes and elements; there is no `innerHTML`,
//! no markdown execution, and no policy. Agent text is untrusted input and is
//! always inserted with `set_text_content` or `create_text_node`.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use web_sys::{
    Document, Element, HtmlButtonElement, HtmlElement, HtmlInputElement, HtmlTextAreaElement,
    KeyboardEvent, Storage,
};

use crate::model::{AppModel, ViewLine};

use super::client::Runtime;

/// Stable references to the rendered layout.
#[derive(Clone)]
pub(crate) struct Elements {
    pub(crate) status: HtmlElement,
    pub(crate) notice: HtmlElement,
    pub(crate) token_input: HtmlInputElement,
    pub(crate) connect_button: HtmlButtonElement,
    pub(crate) disconnect_button: HtmlButtonElement,
    pub(crate) new_session_button: HtmlButtonElement,
    pub(crate) session_list: HtmlElement,
    pub(crate) plan: HtmlElement,
    pub(crate) transcript: HtmlElement,
    pub(crate) approvals: HtmlElement,
    pub(crate) composer: HtmlTextAreaElement,
    pub(crate) send_button: HtmlButtonElement,
    pub(crate) interrupt_button: HtmlButtonElement,
}

pub(crate) fn session_storage() -> Option<Storage> {
    web_sys::window()?.session_storage().ok().flatten()
}

/// Builds the layout once and wires event handlers.
pub(crate) fn start() {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Some(document) = window.document() else {
        return;
    };
    let Some(app) = document.get_element_by_id("app") else {
        return;
    };
    clear(&app);

    let elements = build(&document, &app);
    let model = Rc::new(RefCell::new(AppModel::new()));
    let runtime = Runtime::new(model, elements.clone());

    let search = window.location().search().unwrap_or_default();
    let from_url = search
        .trim_start_matches('?')
        .split('&')
        .find_map(|pair| pair.strip_prefix("token="))
        .map(str::to_string);
    let stored =
        session_storage().and_then(|storage| storage.get_item("ee-web-token").ok().flatten());
    if let Some(token) = from_url.or(stored) {
        elements.token_input.set_value(&token);
    }

    {
        let runtime = runtime.clone();
        let input = elements.token_input.clone();
        let click = Closure::<dyn FnMut()>::new(move || {
            runtime.connect(input.value());
        });
        elements
            .connect_button
            .add_event_listener_with_callback("click", click.as_ref().unchecked_ref())
            .expect("connect listener");
        click.forget();
    }
    {
        let runtime = runtime.clone();
        let click = Closure::<dyn FnMut()>::new(move || runtime.disconnect());
        elements
            .disconnect_button
            .add_event_listener_with_callback("click", click.as_ref().unchecked_ref())
            .expect("disconnect listener");
        click.forget();
    }
    {
        let runtime = runtime.clone();
        let click = Closure::<dyn FnMut()>::new(move || {
            runtime.model.borrow_mut().new_session();
            runtime.flush_outbound();
            runtime.schedule_render();
        });
        elements
            .new_session_button
            .add_event_listener_with_callback("click", click.as_ref().unchecked_ref())
            .expect("new session listener");
        click.forget();
    }
    {
        let runtime = runtime.clone();
        let composer = elements.composer.clone();
        let send = Closure::<dyn FnMut()>::new(move || {
            let text = composer.value();
            runtime.model.borrow_mut().send_task(&text);
            composer.set_value("");
            runtime.flush_outbound();
            runtime.schedule_render();
        });
        elements
            .send_button
            .add_event_listener_with_callback("click", send.as_ref().unchecked_ref())
            .expect("send listener");
        send.forget();
    }
    {
        let runtime = runtime.clone();
        let composer = elements.composer.clone();
        let keydown = Closure::<dyn FnMut(KeyboardEvent)>::new(move |event: KeyboardEvent| {
            if event.key() == "Enter" && !event.shift_key() {
                event.prevent_default();
                let text = composer.value();
                runtime.model.borrow_mut().send_task(&text);
                composer.set_value("");
                runtime.flush_outbound();
                runtime.schedule_render();
            }
        });
        elements
            .composer
            .add_event_listener_with_callback("keydown", keydown.as_ref().unchecked_ref())
            .expect("composer listener");
        keydown.forget();
    }
    {
        let runtime = runtime.clone();
        let click = Closure::<dyn FnMut()>::new(move || {
            runtime.model.borrow_mut().interrupt();
            runtime.flush_outbound();
            runtime.schedule_render();
        });
        elements
            .interrupt_button
            .add_event_listener_with_callback("click", click.as_ref().unchecked_ref())
            .expect("interrupt listener");
        click.forget();
    }

    render(&runtime);
}

fn build(document: &Document, app: &Element) -> Elements {
    let shell = element(document, "div", "shell");
    app.append_child(&shell).expect("shell");

    let header = element(document, "header", "header");
    shell.append_child(&header).expect("header");
    let title = element(document, "h1", "title");
    title.set_text_content(Some("ee agent"));
    header.append_child(&title).expect("title");
    let status = html(document, "div", "status");
    header.append_child(&status).expect("status");
    let notice = html(document, "div", "notice");
    header.append_child(&notice).expect("notice");

    let connect_row = element(document, "div", "connect-row");
    shell.append_child(&connect_row).expect("connect row");
    let token_input: HtmlInputElement = typed(document, "input", "token-input");
    token_input.set_type("password");
    token_input.set_placeholder("bearer token");
    token_input.set_autocomplete("off");
    connect_row.append_child(&token_input).expect("token input");
    let connect_button: HtmlButtonElement = typed(document, "button", "button");
    connect_button.set_text_content(Some("Connect"));
    connect_row.append_child(&connect_button).expect("connect button");
    let disconnect_button: HtmlButtonElement = typed(document, "button", "button secondary");
    disconnect_button.set_text_content(Some("Disconnect"));
    connect_row.append_child(&disconnect_button).expect("disconnect button");

    let body = element(document, "div", "body");
    shell.append_child(&body).expect("body");
    let sidebar = element(document, "aside", "sidebar");
    body.append_child(&sidebar).expect("sidebar");
    let new_session_button: HtmlButtonElement = typed(document, "button", "button");
    new_session_button.set_text_content(Some("New session"));
    sidebar.append_child(&new_session_button).expect("new session");
    let session_list = html(document, "div", "sessions");
    sidebar.append_child(&session_list).expect("sessions");

    let main = element(document, "main", "main");
    body.append_child(&main).expect("main");
    let plan = html(document, "div", "plan");
    main.append_child(&plan).expect("plan");
    let transcript = html(document, "div", "transcript");
    main.append_child(&transcript).expect("transcript");
    let approvals = html(document, "div", "approvals");
    main.append_child(&approvals).expect("approvals");
    let composer_row = element(document, "div", "composer-row");
    main.append_child(&composer_row).expect("composer row");
    let composer: HtmlTextAreaElement = typed(document, "textarea", "composer");
    composer.set_placeholder("Send a task… (Enter sends, Shift+Enter adds a line)");
    composer.set_rows(3);
    composer_row.append_child(&composer).expect("composer");
    let controls = element(document, "div", "controls");
    composer_row.append_child(&controls).expect("controls");
    let send_button: HtmlButtonElement = typed(document, "button", "button");
    send_button.set_text_content(Some("Send"));
    controls.append_child(&send_button).expect("send");
    let interrupt_button: HtmlButtonElement = typed(document, "button", "button secondary");
    interrupt_button.set_text_content(Some("Interrupt"));
    controls.append_child(&interrupt_button).expect("interrupt");

    Elements {
        status,
        notice,
        token_input,
        connect_button,
        disconnect_button,
        new_session_button,
        session_list,
        plan,
        transcript,
        approvals,
        composer,
        send_button,
        interrupt_button,
    }
}

/// Renders the current view state. Called at most once per animation frame.
pub(crate) fn render(runtime: &Runtime) {
    runtime.render_handlers.borrow_mut().clear();
    let view = runtime.model.borrow().view();
    runtime.elements.status.set_text_content(Some(&view.connection));
    runtime.elements.notice.set_text_content(view.notice.as_deref());
    runtime.elements.send_button.set_disabled(!view.can_send);
    runtime.elements.interrupt_button.set_disabled(view.can_send);
    runtime.elements.new_session_button.set_disabled(
        !runtime.model.borrow().connection.eq(&crate::model::ConnectionState::Connected),
    );

    clear(&runtime.elements.session_list);
    let Some(document) = web_sys::window().and_then(|window| window.document()) else {
        return;
    };
    for session in &view.sessions {
        let button: HtmlButtonElement = typed(&document, "button", "session");
        let label = if session.running {
            format!("{} · running", session.label)
        } else {
            session.label.clone()
        };
        button.set_text_content(Some(&label));
        if session.active {
            button.set_class_name("session active");
        }
        let click_runtime = runtime.clone();
        let session_id = session.id.clone();
        let click = Closure::<dyn FnMut()>::new(move || {
            click_runtime.model.borrow_mut().select_session(&session_id);
            click_runtime.flush_outbound();
            click_runtime.schedule_render();
        });
        button
            .add_event_listener_with_callback("click", click.as_ref().unchecked_ref())
            .expect("session listener");
        runtime.render_handlers.borrow_mut().push(click);
        runtime.elements.session_list.append_child(&button).expect("session row");
    }

    clear(&runtime.elements.plan);
    if !view.plan.is_empty() {
        let heading = element(&document, "div", "plan-heading");
        heading.set_text_content(Some("Plan"));
        runtime.elements.plan.append_child(&heading).expect("plan heading");
        for entry in &view.plan {
            let row = element(&document, "div", "plan-entry");
            row.set_text_content(Some(entry));
            runtime.elements.plan.append_child(&row).expect("plan entry");
        }
    }

    clear(&runtime.elements.transcript);
    for line in &view.lines {
        let (class, text) = match line {
            ViewLine::User(text) => ("line user", text.as_str()),
            ViewLine::Assistant(text) => ("line assistant", text.as_str()),
            ViewLine::Thought(text) => ("line thought", text.as_str()),
            ViewLine::Notice(text) => ("line notice", text.as_str()),
            ViewLine::Tool(title, status) => {
                let row = element(&document, "div", "line tool");
                row.set_text_content(Some(&format!("{title} · {status}")));
                runtime.elements.transcript.append_child(&row).expect("tool line");
                continue;
            }
        };
        let row = element(&document, "div", class);
        row.set_text_content(Some(text));
        runtime.elements.transcript.append_child(&row).expect("line");
    }
    runtime.elements.transcript.set_scroll_top(runtime.elements.transcript.scroll_height());

    clear(&runtime.elements.approvals);
    for approval in &view.approvals {
        let card = element(&document, "div", "approval");
        let summary = element(&document, "div", "approval-summary");
        summary.set_text_content(Some(&approval.summary));
        card.append_child(&summary).expect("summary");
        let options = element(&document, "div", "approval-options");
        card.append_child(&options).expect("options");
        for option in &approval.options {
            let button: HtmlButtonElement = typed(&document, "button", "button");
            button.set_text_content(Some(&option.label));
            let click_runtime = runtime.clone();
            let request_id = approval.request_id;
            let index = option.index;
            let click = Closure::<dyn FnMut()>::new(move || {
                click_runtime.model.borrow_mut().answer_approval(request_id, index);
                click_runtime.flush_outbound();
                click_runtime.schedule_render();
            });
            button
                .add_event_listener_with_callback("click", click.as_ref().unchecked_ref())
                .expect("approval listener");
            runtime.render_handlers.borrow_mut().push(click);
            options.append_child(&button).expect("approval option");
        }
        runtime.elements.approvals.append_child(&card).expect("approval card");
    }
}

fn element(document: &Document, tag: &str, class: &str) -> Element {
    let element = document.create_element(tag).expect("create element");
    element.set_class_name(class);
    element
}

fn html(document: &Document, tag: &str, class: &str) -> HtmlElement {
    typed(document, tag, class)
}

fn typed<T: JsCast>(document: &Document, tag: &str, class: &str) -> T {
    let element = document.create_element(tag).expect("create element");
    element.set_class_name(class);
    element.dyn_into::<T>().expect("typed element")
}

fn clear(element: &Element) {
    while let Some(child) = element.first_child() {
        let _ = element.remove_child(&child);
    }
}
