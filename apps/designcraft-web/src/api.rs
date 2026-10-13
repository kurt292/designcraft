//! `window.designcraft`: the desktop control channel (`docs/control-protocol.md`) as promises,
//! so a host page, a browser test or an agent can drive the web app. A same-origin page that
//! embeds the app in an `<iframe>` reaches it as `iframe.contentWindow.designcraft`.
//!
//! ```js
//! await designcraft.request("engine.execute", {command: "frame.create", params: {rect: [36, 36, 300, 200], content: "text"}});
//! const doc = await designcraft.request("document.inspect", {});
//! ```
//!
//! The object is installed once the app has started; `window` then gets a `designcraft-ready`
//! event and `designcraft.ready` is `true`. Every method of the desktop protocol works except the
//! ones that need a window manager (`ui.resize`, `ui.focus`, `app.quit`).
//!
//! Requests are normally handled on the next frame. A tab in the background gets no animation
//! frames, so a timer also drains the queue (`DesignApp::drain_control_now`) while anything is
//! pending; only requests that need a painted frame (`ui.screenshot`) wait for one.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender};

use designcraft_ui_egui::{ControlRequest, ControlResponse, DesignApp};
use serde_json::{Value, json};
use wasm_bindgen::prelude::*;

type Pending = (Receiver<ControlResponse>, js_sys::Function, js_sys::Function);

/// How long to wait for a frame before draining the queue from a timer.
const PUMP_MS: i32 = 150;

thread_local! {
    static TX: RefCell<Option<Sender<ControlRequest>>> = const { RefCell::new(None) };
    static PENDING: RefCell<Vec<Pending>> = const { RefCell::new(Vec::new()) };
    static CTX: RefCell<Option<egui::Context>> = const { RefCell::new(None) };
    static APP: RefCell<Option<Rc<RefCell<DesignApp>>>> = const { RefCell::new(None) };
    static PUMP_ARMED: RefCell<bool> = const { RefCell::new(false) };
}

fn to_js(v: &Value) -> JsValue {
    js_sys::JSON::parse(&v.to_string()).unwrap_or(JsValue::NULL)
}

fn from_js(v: &JsValue) -> Value {
    if v.is_undefined() || v.is_null() {
        return json!({});
    }
    js_sys::JSON::stringify(v).ok().and_then(|s| s.as_string()).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({}))
}

/// Send a control request to the app; the promise resolves with its `result` or rejects with
/// its `error`.
fn request(method: String, params: JsValue) -> js_sys::Promise {
    let params = from_js(&params);
    js_sys::Promise::new(&mut |resolve, reject| {
        let (req, rx) = ControlRequest::new(method.clone(), params.clone());
        let sent = TX.with(|t| t.borrow().as_ref().is_some_and(|tx| tx.send(req).is_ok()));
        if !sent {
            let _ = reject.call1(&JsValue::NULL, &JsValue::from_str("DesignCraft is not running yet"));
            return;
        }
        PENDING.with(|p| p.borrow_mut().push((rx, resolve, reject)));
        CTX.with(|c| {
            if let Some(ctx) = c.borrow().as_ref() {
                ctx.request_repaint();
            }
        });
        arm_pump();
    })
}

/// Schedule one timer that drains the queue if no frame did it first; re-armed while requests
/// are still pending.
fn arm_pump() {
    if PUMP_ARMED.with(|a| std::mem::replace(&mut *a.borrow_mut(), true)) {
        return;
    }
    let Some(window) = web_sys::window() else { return };
    let cb = Closure::once_into_js(|| {
        PUMP_ARMED.with(|a| *a.borrow_mut() = false);
        let still_pending = PENDING.with(|p| !p.borrow().is_empty());
        if !still_pending {
            return;
        }
        let app = APP.with(|a| a.borrow().clone());
        let ctx = CTX.with(|c| c.borrow().clone());
        if let (Some(app), Some(ctx)) = (app, ctx)
            && let Ok(mut app) = app.try_borrow_mut()
        {
            app.drain_control_now(&ctx);
        }
        poll_replies();
        if PENDING.with(|p| !p.borrow().is_empty()) {
            arm_pump();
        }
    });
    if window.set_timeout_with_callback_and_timeout_and_arguments_0(cb.unchecked_ref(), PUMP_MS).is_err() {
        PUMP_ARMED.with(|a| *a.borrow_mut() = false);
    }
}

/// Wire the channel to the app and publish `window.designcraft`.
pub(crate) fn install(tx: Sender<ControlRequest>, ctx: egui::Context, app: Rc<RefCell<DesignApp>>) -> Result<(), JsValue> {
    TX.with(|t| *t.borrow_mut() = Some(tx));
    CTX.with(|c| *c.borrow_mut() = Some(ctx));
    APP.with(|a| *a.borrow_mut() = Some(app));
    let window = web_sys::window().ok_or_else(|| JsValue::from_str("no window"))?;
    let api = js_sys::Object::new();
    let request_fn = Closure::<dyn Fn(JsValue, JsValue) -> js_sys::Promise>::new(|method: JsValue, params: JsValue| {
        request(method.as_string().unwrap_or_default(), params)
    });
    js_sys::Reflect::set(&api, &JsValue::from_str("request"), request_fn.as_ref())?;
    request_fn.forget();
    js_sys::Reflect::set(&api, &JsValue::from_str("version"), &JsValue::from_str(env!("CARGO_PKG_VERSION")))?;
    js_sys::Reflect::set(&api, &JsValue::from_str("ready"), &JsValue::TRUE)?;
    js_sys::Reflect::set(&window, &JsValue::from_str("designcraft"), &api)?;
    let ev = web_sys::Event::new("designcraft-ready")?;
    window.dispatch_event(&ev)?;
    Ok(())
}

/// Settle the promises whose replies arrived (called every frame and by the pump timer).
pub(crate) fn poll_replies() {
    PENDING.with(|p| {
        p.borrow_mut().retain(|(rx, resolve, reject)| match rx.try_recv() {
            Ok(v) => {
                if v["ok"].as_bool() == Some(true) {
                    let _ = resolve.call1(&JsValue::NULL, &to_js(&v["result"]));
                } else {
                    let msg = v["error"].as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
                    let _ = reject.call1(&JsValue::NULL, &JsValue::from_str(&msg));
                }
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                let _ = reject.call1(&JsValue::NULL, &JsValue::from_str("request dropped"));
                false
            }
        });
    });
}
