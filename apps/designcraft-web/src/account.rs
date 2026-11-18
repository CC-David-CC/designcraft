//! The native side of the same-origin WebOS publication channel.

use std::{cell::RefCell, collections::VecDeque, rc::Rc};

use designcraft_engine::cmd::PreparedAccountSave;
use designcraft_ui_egui::DesignApp;
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
        browser.crypto().and_then(|crypto| crypto.get_random_values_with_u8_array(&mut bytes)).map_err(|_| "Could not identify the editor window.")?;
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

pub(crate) struct AccountHost {
    parent: Parent,
    browser: Window,
    inbox: Rc<RefCell<VecDeque<JsValue>>>,
    listener: Closure<dyn FnMut(MessageEvent)>,
    last_state: Value,
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
        browser.add_event_listener_with_callback("message", listener.as_ref().unchecked_ref()).map_err(|_| "Could not receive WebOS document requests.")?;
        Ok(Self { parent, browser, inbox, listener, last_state: Value::Null, pending: None, next_ticket: 1, lease: None, finalized: false })
    }

    pub(crate) fn poll(&mut self, app: &mut DesignApp) {
        let packets = std::mem::take(&mut *self.inbox.borrow_mut());
        for packet in packets {
            if let Err(error) = self.receive(app, &packet) {
                app.status(error);
            }
        }
    }

    pub(crate) fn suspended(&self) -> bool {
        self.lease.is_some() || self.finalized
    }

    fn unfinished(app: &DesignApp) -> bool {
        app.session.tool_composing() || app.session.active().is_some_and(|document| document.interaction.is_some() || document.preview_stash.is_some())
    }

    fn state(&self, app: &DesignApp) -> Value {
        app.session.active().map_or_else(|| json!({"available":false}), |document| {
            let unfinished = Self::unfinished(app);
            let revision = document.interaction.as_ref().map_or(document.revision, |edit| edit.revision).to_string();
            json!({"available":true,"documentId":document.uid.to_string(),"revision":revision,"recoveryRevision":revision,
                "dirty":document.is_dirty(),"capturable":!unfinished,"unfinishedEdit":unfinished,"autosaveEligible":false,"operationPending":self.pending.is_some()})
        })
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
                let result = start_publication(app, packet);
                let reply = match result {
                    Ok(()) => json!({"requestId":id,"ok":true}),
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
            _ => Ok(()),
        }
    }

    fn targets(app: &DesignApp, packet: &JsValue) -> bool {
        app.session.active().is_some_and(|state| {
            let revision = state.interaction.as_ref().map_or(state.revision, |edit| edit.revision);
            number(packet, "documentId") == Some(state.uid) && number(packet, "revision") == Some(revision)
        })
    }

    fn capture_save(&mut self, app: &DesignApp, packet: &JsValue, id: String) -> Result<(), String> {
        let path = field(packet, "path", 2048).ok_or("Invalid DesignCraft save destination.")?;
        let document = number(packet, "documentId").ok_or("Invalid DesignCraft save document.")?;
        let revision = number(packet, "revision").ok_or("Invalid DesignCraft save revision.")?;
        let mut reply = json!({"requestId":id,"path":path,"documentId":document.to_string(),"reviewedRevision":revision.to_string()});
        if self.pending.is_some() || self.suspended() || Self::unfinished(app) || !Self::targets(app, packet) {
            reply["outcome"] = json!("busy");
            return self.parent.send("session-save-result", reply, None);
        }
        match app.session.prepare_account_save(&path) {
            Ok((capture, bytes)) if bytes.len() <= MAX_PROJECT_BYTES as usize => {
                let ticket = self.next_ticket;
                self.next_ticket = ticket.checked_add(1).ok_or("DesignCraft save ticket space exhausted.")?;
                reply["outcome"] = json!("captured");
                reply["ticket"] = json!(ticket.to_string());
                reply["revision"] = json!(revision.to_string());
                self.parent.send("session-save-result", reply, Some(&bytes))?;
                self.pending = Some(Save { ticket, document, revision, path, capture });
                return Ok(());
            }
            Ok(_) => reply["message"] = json!("The publication exceeds the WebOS project size limit."),
            Err(error) => reply["message"] = json!(error.to_string()),
        }
        reply["outcome"] = json!("failed");
        self.parent.send("session-save-result", reply, None)
    }

    fn settle_save(&mut self, app: &mut DesignApp, packet: &JsValue) -> Result<(), String> {
        let matches = self.pending.as_ref().is_some_and(|save| {
            number(packet, "ticket") == Some(save.ticket) && number(packet, "documentId") == Some(save.document)
                && number(packet, "revision") == Some(save.revision) && field(packet, "path", 2048).as_deref() == Some(save.path.as_str())
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
        self.parent.send("save-settled", json!({"ticket":save.ticket.to_string(),"documentId":save.document.to_string(),
            "revision":save.revision.to_string(),"path":save.path,"adopted":adopted,"projectState":self.state(app)}), None)
    }

    fn session(&mut self, app: &DesignApp, packet: &JsValue, id: String) -> Result<(), String> {
        let operation = field(packet, "operation", 16).ok_or("Invalid DesignCraft session operation.")?;
        let lease = field(packet, "lease", 64).filter(|lease| !lease.is_empty()).ok_or("Invalid DesignCraft session lease.")?;
        let accepted = match operation.as_str() {
            "pause" if !self.suspended() && self.pending.is_none() => {
                let dirty = app.session.active().map(|document| document.is_dirty());
                if !Self::targets(app, packet) || dirty != get(packet, "dirty").as_bool()
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
        self.parent.send("session-result", json!({"requestId":id,"operation":operation,"lease":lease,"ok":accepted,
            "message":if accepted {""} else {"The DesignCraft publication changed after close review."}}), None)
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
        app.session.execute("file.new", &json!({})).map_err(|error| error.to_string())?;
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
