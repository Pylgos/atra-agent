use dioxus::prelude::*;
use gloo_net::http::Request;
use wasm_bindgen_futures::JsFuture;

#[wasm_bindgen::prelude::wasm_bindgen(inline_js = r#"
export function pushSupported() {
  return window.isSecureContext &&
    "Notification" in window &&
    "serviceWorker" in navigator &&
    "PushManager" in window;
}

export function notificationPermission() {
  return "Notification" in window ? window.Notification.permission : "unsupported";
}

function subscriptionJson(subscription) {
  const json = subscription.toJSON();
  return JSON.stringify({ endpoint: subscription.endpoint, keys: json.keys });
}

export async function currentPushSubscription() {
  const registration = await navigator.serviceWorker.getRegistration("/");
  const subscription = registration && await registration.pushManager.getSubscription();
  return subscription ? subscriptionJson(subscription) : "";
}

function applicationServerKey(encoded) {
  const padding = "=".repeat((4 - encoded.length % 4) % 4);
  const base64 = (encoded + padding).replace(/-/g, "+").replace(/_/g, "/");
  return Uint8Array.from(atob(base64), character => character.charCodeAt(0));
}

export async function subscribePush(publicKey) {
  if (window.Notification.permission === "denied") {
    throw new Error("notifications are blocked in browser settings");
  }
  if (window.Notification.permission !== "granted") {
    const permission = await window.Notification.requestPermission();
    if (permission !== "granted") {
      throw new Error("notification permission was not granted");
    }
  }
  await navigator.serviceWorker.register("/service-worker.js");
  const registration = await navigator.serviceWorker.ready;
  let subscription = await registration.pushManager.getSubscription();
  if (!subscription) {
    subscription = await registration.pushManager.subscribe({
      userVisibleOnly: true,
      applicationServerKey: applicationServerKey(publicKey),
    });
  }
  return subscriptionJson(subscription);
}

export async function unsubscribePush() {
  const registration = await navigator.serviceWorker.getRegistration("/");
  const subscription = registration && await registration.pushManager.getSubscription();
  if (!subscription) return "";
  const json = subscriptionJson(subscription);
  await subscription.unsubscribe();
  return json;
}
"#)]
extern "C" {
    #[wasm_bindgen::prelude::wasm_bindgen(js_name = pushSupported)]
    fn push_supported() -> bool;
    #[wasm_bindgen::prelude::wasm_bindgen(js_name = notificationPermission)]
    fn notification_permission() -> String;
    #[wasm_bindgen::prelude::wasm_bindgen(js_name = currentPushSubscription)]
    fn current_push_subscription_js() -> js_sys::Promise;
    #[wasm_bindgen::prelude::wasm_bindgen(js_name = subscribePush)]
    fn subscribe_push_js(public_key: &str) -> js_sys::Promise;
    #[wasm_bindgen::prelude::wasm_bindgen(js_name = unsubscribePush)]
    fn unsubscribe_push_js() -> js_sys::Promise;
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct BrowserPushSubscription {
    endpoint: String,
    keys: BrowserPushKeys,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct BrowserPushKeys {
    auth: String,
    p256dh: String,
}

#[derive(serde::Deserialize)]
struct PushKey {
    public_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PushState {
    Checking,
    Unsupported,
    Denied,
    Available,
}

impl PushState {
    fn disabled(&self, busy: bool) -> bool {
        busy || !matches!(self, Self::Available)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PushSnapshot {
    state: PushState,
    subscribed: bool,
    message: String,
}

#[component]
pub(crate) fn PushSettings() -> Element {
    let mut snapshot = use_signal(|| PushSnapshot {
        state: PushState::Checking,
        subscribed: false,
        message: "Checking Web Push…".to_owned(),
    });
    let mut busy = use_signal(|| false);
    use_effect(move || {
        spawn(async move {
            snapshot.set(current_push_state().await);
        });
    });

    rsx! {
        label { class: "check-row",
            input {
                r#type: "checkbox",
                checked: snapshot.read().subscribed,
                disabled: snapshot.read().state.disabled(busy()),
                onchange: move |event| {
                    let enabled = event.checked();
                    busy.set(true);
                    snapshot.write().message = if enabled {
                        "Enabling Web Push…".to_owned()
                    } else {
                        "Disabling Web Push…".to_owned()
                    };
                    spawn(async move {
                        snapshot.set(match set_push_subscription(enabled).await {
                            Ok(current) => available_snapshot(current),
                            Err(error) => {
                                let mut current = current_push_state().await;
                                if matches!(current.state, PushState::Available) {
                                    current.message = error;
                                }
                                current
                            }
                        });
                        busy.set(false);
                    });
                }
            }
            "Web Push notifications"
        }
        small { role: "status", "{snapshot.read().message}" }
        div { class: "button-row",
            button {
                disabled: !snapshot.read().subscribed || busy(),
                onclick: move |_| {
                    busy.set(true);
                    snapshot.write().message = "Sending test notification…".to_owned();
                    spawn(async move {
                        snapshot.write().message = match send_test_push().await {
                            Ok(()) => "Test notification sent.".to_owned(),
                            Err(error) => error,
                        };
                        busy.set(false);
                    });
                },
                "Send test notification"
            }
        }
    }
}

async fn current_push_state() -> PushSnapshot {
    if !push_supported() {
        return PushSnapshot {
            state: PushState::Unsupported,
            subscribed: false,
            message: "Web Push requires HTTPS and a supported browser.".to_owned(),
        };
    }
    if notification_permission() == "denied" {
        return PushSnapshot {
            state: PushState::Denied,
            subscribed: false,
            message: "Notifications are blocked in browser settings.".to_owned(),
        };
    }
    match current_push_subscription().await {
        Ok(Some(_)) => available_snapshot(true),
        Ok(None) => available_snapshot(false),
        Err(error) => PushSnapshot {
            state: PushState::Available,
            subscribed: false,
            message: error,
        },
    }
}

fn available_snapshot(subscribed: bool) -> PushSnapshot {
    PushSnapshot {
        state: PushState::Available,
        subscribed,
        message: if subscribed {
            "This browser is subscribed.".to_owned()
        } else {
            "Web Push is disabled.".to_owned()
        },
    }
}

fn js_error(value: wasm_bindgen::JsValue) -> String {
    value.as_string().unwrap_or_else(|| format!("{value:?}"))
}

async fn current_push_subscription() -> Result<Option<BrowserPushSubscription>, String> {
    let value = JsFuture::from(current_push_subscription_js())
        .await
        .map_err(js_error)?;
    let encoded = value.as_string().unwrap_or_default();
    if encoded.is_empty() {
        Ok(None)
    } else {
        serde_json::from_str(&encoded)
            .map(Some)
            .map_err(|error| error.to_string())
    }
}

async fn set_push_subscription(enabled: bool) -> Result<bool, String> {
    if enabled {
        if notification_permission() == "denied" {
            return Err("Notifications are blocked in browser settings.".to_owned());
        }
        let key = Request::get("/api/push/key")
            .send()
            .await
            .map_err(|error| error.to_string())?
            .json::<PushKey>()
            .await
            .map_err(|error| error.to_string())?;
        let encoded = JsFuture::from(subscribe_push_js(&key.public_key))
            .await
            .map_err(js_error)?
            .as_string()
            .ok_or_else(|| "browser returned an invalid Push subscription".to_owned())?;
        let subscription: BrowserPushSubscription =
            serde_json::from_str(&encoded).map_err(|error| error.to_string())?;
        let response = Request::put("/api/push/subscription")
            .header("Content-Type", "application/json")
            .json(&subscription)
            .map_err(|error| error.to_string())?
            .send()
            .await
            .map_err(|error| error.to_string())?;
        if !response.ok() {
            return Err(format!(
                "subscription registration failed ({})",
                response.status()
            ));
        }
        Ok(true)
    } else {
        if let Some(subscription) = current_push_subscription().await? {
            let response = Request::delete("/api/push/subscription")
                .header("Content-Type", "application/json")
                .json(&serde_json::json!({"endpoint": subscription.endpoint}))
                .map_err(|error| error.to_string())?
                .send()
                .await
                .map_err(|error| error.to_string())?;
            if !response.ok() {
                return Err(format!(
                    "subscription removal failed ({})",
                    response.status()
                ));
            }
            JsFuture::from(unsubscribe_push_js())
                .await
                .map_err(js_error)?;
        }
        Ok(false)
    }
}

async fn send_test_push() -> Result<(), String> {
    let subscription = current_push_subscription()
        .await?
        .ok_or_else(|| "Enable Web Push first.".to_owned())?;
    let response = Request::post("/api/push/test")
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({"endpoint": subscription.endpoint}))
        .map_err(|error| error.to_string())?
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if response.ok() {
        Ok(())
    } else {
        Err(format!("test notification failed ({})", response.status()))
    }
}
