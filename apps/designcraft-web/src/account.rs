//! The native side of the same-origin WebOS publication channel.

use std::{cell::RefCell, collections::VecDeque, rc::Rc};

use designcraft_engine::cmd::PreparedAccountSave;
use designcraft_ui_egui::{AccountDocumentFn, AccountDocumentRequest, DesignApp};
use js_sys::{Array, ArrayBuffer, Object, Reflect, Uint8Array};
use serde_json::{Value, json};
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use web_sys::{MessageEvent, Window};

const PREFIX: &str = "designcraft:account-";
const MAX_PROJECT_BYTES: u32 = 128 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct Parent {
    window: Window,
    origin: String,
    generation: String,
}

impl Parent {
    pub(crate) fn connect() -> Result<Self, String> {
        let browser = web_sys::window().ok_or("No browser window.")?;
        let window = browser.parent().ok().flatten().ok_or("No parent Desktop.")?;
        let origin = browser.location().origin().map_err(|_| "No browser origin.")?;
        if Object::is(browser.as_ref(), window.as_ref()) || origin == "null" || window.location().origin().ok().as_ref() != Some(&origin) {
            return Err("DesignCraft requires a same-origin WebOS window.".into());
        }
        let mut bytes = [0u8; 16];
        browser
            .crypto()
            .and_then(|crypto| crypto.get_random_values_with_u8_array(&mut bytes))
            .map_err(|_| "Could not identify the editor window.")?;
        let generation = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(Self { window, origin, generation })
    }

    pub(crate) fn ready(&self, error: Option<&str>) -> Result<(), String> {
        match error {
            Some(message) => self.send("failed", json!({"message":message}), None),
            None => self.send("ready", json!({}), None),
        }
    }

    fn send(&self, kind: &str, mut packet: Value, bytes: Option<&[u8]>) -> Result<(), String> {
        packet["type"] = Value::String(format!("{PREFIX}{kind}"));
        packet["generation"] = Value::String(self.generation.clone());
        let packet = js_sys::JSON::parse(&packet.to_string()).map_err(|_| "Could not encode DesignCraft's response.")?;
        let transfer = Array::new();
        if let Some(bytes) = bytes {
            if bytes.len() > MAX_PROJECT_BYTES as usize {
                return Err("The DesignCraft project exceeds the WebOS project size limit.".into());
            }
            let buffer = Uint8Array::from(bytes).buffer();
            if Reflect::set(&packet, &JsValue::from_str("bytes"), &buffer).ok() != Some(true) {
                return Err("Could not send DesignCraft's publication bytes.".into());
            }
            transfer.push(&buffer);
        }
        self.window.post_message_with_transfer(&packet, &self.origin, &transfer).map_err(|_| "Could not reach the WebOS Desktop.".into())
    }
}

fn get(packet: &JsValue, name: &str) -> JsValue {
    Reflect::get(packet, &JsValue::from_str(name)).unwrap_or(JsValue::UNDEFINED)
}

fn field(packet: &JsValue, name: &str, limit: usize) -> Option<String> {
    get(packet, name).as_string().filter(|value| value.len() <= limit)
}

fn number(packet: &JsValue, name: &str) -> Option<u64> {
    field(packet, name, 20)?.parse().ok()
}

struct Save {
    ticket: u64,
    document: u64,
    revision: u64,
    path: String,
    capture: PreparedAccountSave,
}

struct Dialog {
    id: u64,
    action: DialogAction,
}

enum DialogAction {
    Navigate,
    Save { document: u64, revision: u64 },
    Relink { document: u64, revision: u64, asset: u64 },
}

pub(crate) struct AccountHost {
    parent: Parent,
    browser: Window,
    inbox: Rc<RefCell<VecDeque<JsValue>>>,
    listener: Closure<dyn FnMut(MessageEvent)>,
    last_state: Value,
    action: Rc<RefCell<Option<AccountDocumentRequest>>>,
    dialog: Option<Dialog>,
    next_dialog: u64,
    pending: Option<Save>,
    next_ticket: u64,
    lease: Option<String>,
    finalized: bool,
}

impl AccountHost {
    pub(crate) fn install(parent: Parent, context: egui::Context) -> Result<Self, String> {
        let browser = web_sys::window().ok_or("No browser window.")?;
        let inbox = Rc::new(RefCell::new(VecDeque::new()));
        let pending = inbox.clone();
        let endpoint = parent.clone();
        let listener = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            if event.origin() != endpoint.origin || !event.source().is_some_and(|source| Object::is(source.as_ref(), endpoint.window.as_ref())) {
                return;
            }
            let packet = event.data();
            if field(&packet, "generation", 32).as_deref() != Some(&endpoint.generation)
                || !field(&packet, "type", 64).is_some_and(|kind| kind.starts_with(PREFIX))
            {
                return;
            }
            if let Ok(mut pending) = pending.try_borrow_mut()
                && pending.len() < 8
            {
                pending.push_back(packet);
                context.request_repaint();
            }
        });
        browser
            .add_event_listener_with_callback("message", listener.as_ref().unchecked_ref())
            .map_err(|_| "Could not receive WebOS document requests.")?;
        Ok(Self {
            parent,
            browser,
            inbox,
            listener,
            last_state: Value::Null,
            action: Rc::default(),
            dialog: None,
            next_dialog: 1,
            pending: None,
            next_ticket: 1,
            lease: None,
            finalized: false,
        })
    }

    pub(crate) fn document_service(&self) -> AccountDocumentFn {
        let action = self.action.clone();
        Box::new(move |request| {
            let mut next = action.try_borrow_mut().map_err(|_| "DesignCraft is processing a document request.")?;
            if next.is_some() {
                return Err("DesignCraft is processing a document request.".into());
            }
            *next = Some(request);
            Ok(())
        })
    }

    pub(crate) fn poll(&mut self, app: &mut DesignApp) {
        let packets = std::mem::take(&mut *self.inbox.borrow_mut());
        for packet in packets {
            if let Err(error) = self.receive(app, &packet) {
                app.status(error);
            }
        }
        let action = self.action.borrow_mut().take();
        if let Some(action) = action
            && let Err(error) = self.document_action(app, action)
        {
            app.status(error);
        }
    }

    pub(crate) fn suspended(&self) -> bool {
        self.lease.is_some() || self.finalized
    }

    fn unfinished(app: &DesignApp) -> bool {
        app.session.tool_composing()
            || app.session.active().is_some_and(|document| document.interaction.is_some() || document.preview_stash.is_some())
    }

    fn state(&self, app: &DesignApp) -> Value {
        app.session.active().map_or_else(
            || json!({"available":false}),
            |document| {
                let unfinished = Self::unfinished(app);
                let revision = document.interaction.as_ref().map_or(document.revision, |edit| edit.revision).to_string();
                json!({"available":true,"documentId":document.uid.to_string(),"revision":revision,"recoveryRevision":revision,
                "dirty":document.is_dirty(),"capturable":!unfinished,"unfinishedEdit":unfinished,"autosaveEligible":false,
                "operationPending":self.pending.is_some() || self.dialog.is_some()})
            },
        )
    }

    pub(crate) fn publish_state(&mut self, app: &DesignApp) -> Result<(), String> {
        let state = self.state(app);
        if self.last_state != state {
            self.parent.send("project-state", state.clone(), None)?;
            self.last_state = state;
        }
        Ok(())
    }

    fn receive(&mut self, app: &mut DesignApp, packet: &JsValue) -> Result<(), String> {
        let kind = field(packet, "type", 64);
        let kind = kind.as_deref().and_then(|kind| kind.strip_prefix(PREFIX));
        if kind == Some("save-result") {
            return self.settle_save(app, packet);
        }
        let Some(id) = field(packet, "requestId", 36) else { return Ok(()) };
        match kind {
            Some("start") => {
                let reply = match start_publication(app, packet) {
                    Ok(()) => {
                        let document = app.session.active().ok_or("DesignCraft did not open a publication.")?;
                        let mut state = self.state(app);
                        state["requestId"] = json!(id);
                        state["ok"] = json!(true);
                        state["path"] = json!(document.path);
                        state
                    }
                    Err(message) => json!({"requestId":id,"ok":false,"message":message}),
                };
                self.parent.send("started", reply, None)
            }
            Some("recovery-capture") => {
                let document = field(packet, "documentId", 20);
                let revision = field(packet, "revision", 20);
                let mut reply = json!({"requestId":id,"documentId":document,"revision":revision});
                let matches = !self.suspended() && !Self::unfinished(app) && Self::targets(app, packet);
                if matches {
                    let bytes = app.session.account_recovery_snapshot().map_err(|error| error.to_string());
                    match bytes {
                        Ok(bytes) if bytes.len() <= MAX_PROJECT_BYTES as usize => return self.parent.send("recovery-captured", reply, Some(&bytes)),
                        Ok(_) => reply["message"] = json!("The DesignCraft draft exceeds the WebOS project size limit."),
                        Err(message) => reply["message"] = json!(message),
                    }
                } else {
                    reply["message"] = json!("The DesignCraft publication changed before recovery capture.");
                }
                self.parent.send("recovery-captured", reply, None)
            }
            Some("session-save") => self.capture_save(app, packet, id),
            Some("session") => self.session(app, packet, id),
            Some("dialog-result") => self.dialog_result(app, packet),
            Some("operation") => self.operation(app, packet, id),
            _ => Ok(()),
        }
    }

    fn targets(app: &DesignApp, packet: &JsValue) -> bool {
        app.session.active().is_some_and(|state| {
            let revision = state.interaction.as_ref().map_or(state.revision, |edit| edit.revision);
            number(packet, "documentId") == Some(state.uid) && number(packet, "revision") == Some(revision)
        })
    }

    fn operation(&self, app: &mut DesignApp, packet: &JsValue, id: String) -> Result<(), String> {
        let document = field(packet, "documentId", 20).ok_or("Invalid DesignCraft document target.")?;
        let mut reply = json!({"requestId":id,"documentId":document});
        match self.run_operation(app, packet) {
            Ok(value) => reply["result"] = json!(value.to_string()),
            Err(error) => reply["message"] = json!(error),
        }
        self.parent.send("operation-result", reply, None)
    }

    fn run_operation(&self, app: &mut DesignApp, packet: &JsValue) -> Result<Value, String> {
        let path = get(packet, "path");
        let path = if path.is_null() { None } else { Some(path.as_string().ok_or("Invalid DesignCraft document binding.")?) };
        let state = app.session.active().ok_or("No DesignCraft publication is open.")?;
        if self.suspended() || number(packet, "documentId") != Some(state.uid) || path != state.path {
            return Err("The DesignCraft operation does not match the current publication.".into());
        }
        let command = field(packet, "command", 64).ok_or("Invalid DesignCraft operation.")?;
        let args = field(packet, "args", 4096).ok_or("Invalid DesignCraft operation arguments.")?;
        let args = serde_json::from_str::<Value>(&args).map_err(|_| "Invalid DesignCraft operation arguments.")?;
        let object = args.as_object().ok_or("DesignCraft operation arguments must be an object.")?;
        if command == "document.inspect" && object.is_empty() {
            let graphics: Vec<_> = state
                .doc
                .assets
                .values()
                .take(100)
                .map(|graphic| json!({"id":graphic.id.0,"name":graphic.name,"pixels":graphic.pixels,"storage":graphic.storage,"mime":graphic.mime}))
                .collect();
            return Ok(json!({"title":state.title(),"pages":state.doc.page_count(),"stories":state.doc.stories.len(),
                "dirty":state.is_dirty(),"graphics":graphics}));
        }
        if self.pending.is_some() || self.dialog.is_some() || Self::unfinished(app) {
            return Err("Finish the pending DesignCraft edit or operation first.".into());
        }
        match command.as_str() {
            "frame.create" if object.len() == 2 => {
                let valid_rect =
                    object.get("rect").and_then(Value::as_array).is_some_and(|rect| rect.len() == 4 && rect.iter().all(Value::is_number));
                let text = object.get("text").and_then(Value::as_str).filter(|text| !text.is_empty() && text.len() <= 2048);
                match (valid_rect, text) {
                    (true, Some(text)) => app.run("frame.create", json!({"rect":object["rect"],"text":text,"content":"text","caret":false})),
                    _ => Err("Choose a rectangle and text for the DesignCraft frame.".into()),
                }
            }
            "links.relink" if object.len() == 1 && object.get("asset").and_then(Value::as_u64).is_some() => app.run("links.relink", args),
            "pdf.export" if object.is_empty() => {
                let title = state.title();
                let name = std::path::Path::new(&title).file_stem().and_then(|name| name.to_str()).unwrap_or("Publication");
                app.run("app.exportPdf", json!({"path":format!("{name}.pdf")}))
            }
            "edit.undo" if object.is_empty() => app.run("edit.undo", json!({})),
            _ => Err("Unsupported DesignCraft document operation.".into()),
        }
    }

    fn capture_save(&mut self, app: &DesignApp, packet: &JsValue, id: String) -> Result<(), String> {
        let path = field(packet, "path", 2048).ok_or("Invalid DesignCraft save destination.")?;
        let document = number(packet, "documentId").ok_or("Invalid DesignCraft save document.")?;
        let revision = number(packet, "revision").ok_or("Invalid DesignCraft save revision.")?;
        let mut reply = json!({"requestId":id,"path":path,"documentId":document.to_string(),"reviewedRevision":revision.to_string()});
        if self.pending.is_some() || self.dialog.is_some() || self.suspended() || Self::unfinished(app) || !Self::targets(app, packet) {
            reply["outcome"] = json!("busy");
            return self.parent.send("session-save-result", reply, None);
        }
        if let Err(error) = self.send_save(app, path, "session-save-result", reply.clone()) {
            reply["message"] = json!(error);
            reply["outcome"] = json!("failed");
            self.parent.send("session-save-result", reply, None)?;
        }
        Ok(())
    }

    fn send_save(&mut self, app: &DesignApp, path: String, kind: &str, mut reply: Value) -> Result<(), String> {
        let state = app.session.active().ok_or("No DesignCraft publication is open.")?;
        let (capture, bytes) = app.session.prepare_account_save(&path).map_err(|error| error.to_string())?;
        if bytes.len() > MAX_PROJECT_BYTES as usize {
            return Err("The publication exceeds the WebOS project size limit.".into());
        }
        let ticket = self.next_ticket;
        self.next_ticket = ticket.checked_add(1).ok_or("DesignCraft save ticket space exhausted.")?;
        reply["outcome"] = json!("captured");
        reply["ticket"] = json!(ticket.to_string());
        reply["documentId"] = json!(state.uid.to_string());
        reply["revision"] = json!(state.revision.to_string());
        reply["path"] = json!(path);
        self.parent.send(kind, reply, Some(&bytes))?;
        self.pending = Some(Save { ticket, document: state.uid, revision: state.revision, path, capture });
        Ok(())
    }

    fn document_action(&mut self, app: &DesignApp, action: AccountDocumentRequest) -> Result<(), String> {
        if self.suspended() || self.pending.is_some() || self.dialog.is_some() {
            return Err("Finish the pending DesignCraft document operation first.".into());
        }
        let state = app.session.active().ok_or("No DesignCraft publication is open.")?;
        match action {
            AccountDocumentRequest::Close => self.parent.send("close-intent", json!({}), None),
            AccountDocumentRequest::New(settings) => {
                let settings = if settings.is_object() { settings } else { json!({}) };
                self.request_dialog(json!({"operation":"new","settings":settings.to_string()}), DialogAction::Navigate)
            }
            AccountDocumentRequest::Open => self.request_dialog(json!({"operation":"open","path":null}), DialogAction::Navigate),
            AccountDocumentRequest::RelinkGraphic(asset) => {
                if Self::unfinished(app) {
                    return Err("Finish the active DesignCraft edit before relinking a graphic.".into());
                }
                self.request_dialog(json!({"operation":"relink"}), DialogAction::Relink { document: state.uid, revision: state.revision, asset })
            }
            AccountDocumentRequest::Save | AccountDocumentRequest::SaveAs => {
                if Self::unfinished(app) {
                    return Err("Finish the active DesignCraft edit before saving.".into());
                }
                if matches!(action, AccountDocumentRequest::Save)
                    && let Some(path) = &state.path
                {
                    return self.send_save(app, path.clone(), "save", json!({}));
                }
                let suggested = state.path.clone().unwrap_or_else(|| designcraft_engine::cmd::with_document_extension(&state.doc.title));
                self.request_dialog(
                    json!({"operation":"save","suggested":suggested}),
                    DialogAction::Save { document: state.uid, revision: state.revision },
                )
            }
        }
    }

    fn request_dialog(&mut self, mut packet: Value, action: DialogAction) -> Result<(), String> {
        let id = self.next_dialog;
        self.next_dialog = id.checked_add(1).ok_or("DesignCraft dialog space exhausted.")?;
        packet["requestId"] = json!(id.to_string());
        self.parent.send("dialog", packet, None)?;
        self.dialog = Some(Dialog { id, action });
        Ok(())
    }

    fn dialog_result(&mut self, app: &DesignApp, packet: &JsValue) -> Result<(), String> {
        if self.dialog.as_ref().map(|dialog| dialog.id) != number(packet, "requestId") {
            return Ok(());
        }
        let Some(dialog) = self.dialog.take() else { return Ok(()) };
        let outcome = field(packet, "outcome", 16);
        let current = app.session.active().map(|state| (state.uid, state.revision));
        match (outcome.as_deref(), dialog.action) {
            (Some("failed"), _) => Err(field(packet, "message", 250).unwrap_or_else(|| "The WebOS document dialog failed.".into())),
            (Some("destination"), DialogAction::Save { document, revision }) => {
                if current != Some((document, revision)) || Self::unfinished(app) || self.suspended() {
                    return Err("The publication changed while choosing its save destination.".into());
                }
                let path = field(packet, "path", 2048).ok_or("Invalid DesignCraft destination.")?;
                self.send_save(app, path, "save", json!({}))
            }
            (Some("asset"), DialogAction::Relink { document, revision, asset }) => {
                if current != Some((document, revision)) || Self::unfinished(app) || self.suspended() {
                    return Err("The publication changed while choosing its graphic.".into());
                }
                let name = field(packet, "name", 255).ok_or("Invalid account graphic filename.")?;
                let bytes = get(packet, "bytes").dyn_into::<ArrayBuffer>().map_err(|_| "Invalid account graphic data.")?;
                if bytes.byte_length() == 0 || bytes.byte_length() > MAX_PROJECT_BYTES {
                    return Err("Invalid account graphic size.".into());
                }
                app.session.embed_account_graphic(asset, &name, Uint8Array::new(&bytes).to_vec()).map_err(|error| error.to_string())?;
                app.after_engine();
                app.status(format!("Embedded {name} in the publication."));
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn settle_save(&mut self, app: &mut DesignApp, packet: &JsValue) -> Result<(), String> {
        let matches = self.pending.as_ref().is_some_and(|save| {
            number(packet, "ticket") == Some(save.ticket)
                && number(packet, "documentId") == Some(save.document)
                && number(packet, "revision") == Some(save.revision)
                && field(packet, "path", 2048).as_deref() == Some(save.path.as_str())
        });
        if !matches {
            return Ok(());
        }
        let outcome = field(packet, "outcome", 16);
        if !matches!(outcome.as_deref(), Some("written" | "failed")) {
            return Ok(());
        }
        let Some(save) = self.pending.take() else { return Ok(()) };
        let adopted = outcome.as_deref() == Some("written") && app.session.adopt_account_save(save.capture).is_ok();
        if adopted {
            app.after_engine();
        }
        self.parent.send(
            "save-settled",
            json!({"ticket":save.ticket.to_string(),"documentId":save.document.to_string(),
            "revision":save.revision.to_string(),"path":save.path,"adopted":adopted,"projectState":self.state(app)}),
            None,
        )
    }

    fn session(&mut self, app: &DesignApp, packet: &JsValue, id: String) -> Result<(), String> {
        let operation = field(packet, "operation", 16).ok_or("Invalid DesignCraft session operation.")?;
        let lease = field(packet, "lease", 64).filter(|lease| !lease.is_empty()).ok_or("Invalid DesignCraft session lease.")?;
        let accepted = match operation.as_str() {
            "pause" if !self.suspended() && self.pending.is_none() && self.dialog.is_none() => {
                let dirty = app.session.active().map(|document| document.is_dirty());
                if !Self::targets(app, packet)
                    || dirty != get(packet, "dirty").as_bool()
                    || Some(Self::unfinished(app)) != get(packet, "unfinishedEdit").as_bool()
                {
                    false
                } else {
                    self.lease = Some(lease.clone());
                    true
                }
            }
            "release" | "finalize" if self.lease.as_deref() == Some(lease.as_str()) => {
                self.lease = None;
                self.finalized = operation == "finalize";
                true
            }
            _ => false,
        };
        self.parent.send(
            "session-result",
            json!({"requestId":id,"operation":operation,"lease":lease,"ok":accepted,
            "message":if accepted {""} else {"The DesignCraft publication changed after close review."}}),
            None,
        )
    }
}

impl Drop for AccountHost {
    fn drop(&mut self) {
        let _ = self.browser.remove_event_listener_with_callback("message", self.listener.as_ref().unchecked_ref());
    }
}

fn start_publication(app: &mut DesignApp, packet: &JsValue) -> Result<(), String> {
    if !app.session.documents().is_empty() {
        return Err("This WebOS window already owns a DesignCraft publication.".into());
    }
    let path = get(packet, "path");
    let path = if path.is_null() { None } else { Some(path.as_string().ok_or("Invalid account publication path.")?) };
    let recover = get(packet, "recover").as_bool().ok_or("Invalid DesignCraft recovery request.")?;
    let bytes = get(packet, "bytes");
    if bytes.is_null() && path.is_none() && !recover {
        let settings = match field(packet, "newSettings", 4096) {
            Some(value) => serde_json::from_str::<Value>(&value).map_err(|_| "Invalid DesignCraft publication settings.")?,
            None => json!({}),
        };
        let command = if settings["sample"] == true { "file.newSample" } else { "file.new" };
        app.session.execute(command, &settings).map_err(|error| error.to_string())?;
        app.after_engine();
        return Ok(());
    }
    let bytes = bytes.dyn_into::<ArrayBuffer>().map_err(|_| "Invalid DesignCraft publication bytes.")?;
    if bytes.byte_length() == 0 || bytes.byte_length() > MAX_PROJECT_BYTES {
        return Err("Invalid DesignCraft publication size.".into());
    }
    let warnings = app.adopt_account_document(path.as_deref(), &Uint8Array::new(&bytes).to_vec(), recover)?;
    if !warnings.is_empty() {
        app.status(warnings.join(". "));
    }
    Ok(())
}
