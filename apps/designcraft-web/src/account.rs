//! The native side of the same-origin WebOS publication channel.

use std::{cell::RefCell, collections::VecDeque, rc::Rc};

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

pub(crate) struct AccountHost {
    parent: Parent,
    browser: Window,
    inbox: Rc<RefCell<VecDeque<JsValue>>>,
    listener: Closure<dyn FnMut(MessageEvent)>,
    last_state: Value,
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
        Ok(Self { parent, browser, inbox, listener, last_state: Value::Null })
    }

    pub(crate) fn poll(&mut self, app: &mut DesignApp) {
        let packets = std::mem::take(&mut *self.inbox.borrow_mut());
        for packet in packets {
            if let Err(error) = self.receive(app, &packet) {
                app.status(error);
            }
        }
    }

    pub(crate) fn publish_state(&mut self, app: &DesignApp) -> Result<(), String> {
        let state = app.session.active().map_or_else(|| json!({"available":false}), |document| {
            let unfinished = document.interaction.is_some() || document.preview_stash.is_some();
            let revision = document.revision.to_string();
            json!({"available":true,"documentId":document.uid.to_string(),"revision":revision,"recoveryRevision":revision,
                "dirty":document.is_dirty(),"capturable":!unfinished,"unfinishedEdit":unfinished,"autosaveEligible":false,"operationPending":false})
        });
        if self.last_state != state {
            self.parent.send("project-state", state.clone(), None)?;
            self.last_state = state;
        }
        Ok(())
    }

    fn receive(&self, app: &mut DesignApp, packet: &JsValue) -> Result<(), String> {
        let Some(id) = field(packet, "requestId", 36) else { return Ok(()) };
        match field(packet, "type", 64).as_deref().and_then(|kind| kind.strip_prefix(PREFIX)) {
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
                let matches = app.session.active().is_some_and(|state| {
                    document.as_deref() == Some(&state.uid.to_string()) && revision.as_deref() == Some(&state.revision.to_string())
                });
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
            _ => Ok(()),
        }
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
