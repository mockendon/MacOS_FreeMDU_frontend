// FreeMDU for macOS: Tauri backend around the `freemdu` protocol crate.
// Mirrors the official TUI's worker: one thread owns the serial port, connects
// with a 1 s timeout, retries every 4 s, polls property kinds at staggered
// rates (polling also keeps the appliance's 3 s unlock alive) and runs actions.

use std::{sync::Mutex, time::Duration};

use freemdu::{
    device::{self, ActionParameters, Date, Device, Error, Fault, FaultInfo, PropertyKind, Value},
    serial::{self, Port},
};
use serde_json::{json, Map, Value as Json};
use tauri::{AppHandle, Emitter, State};
use tokio::{
    sync::{mpsc, mpsc::error::TryRecvError, oneshot},
    time,
};

const DEVICE_TIMEOUT: Duration = Duration::from_secs(1);
const DEVICE_CONNECT_INTERVAL: Duration = Duration::from_secs(4);

type Dev<'a> = Box<dyn Device<&'a mut Port> + 'a>;

enum Cmd {
    Action { id: String, param: Option<String>, reply: oneshot::Sender<Result<(), String>> },
    Stop,
}

enum Outcome {
    Stop,
    Lost(String),
}

#[derive(Default)]
struct Session(Mutex<Option<mpsc::UnboundedSender<Cmd>>>);

fn val(v: &Value) -> Json {
    match v {
        Value::Bool(b) => json!({ "t": "bool", "v": b }),
        Value::Number(n) => json!({ "t": "number", "v": n }),
        Value::Sensor(cur, target) => json!({ "t": "sensor", "v": [cur, target] }),
        Value::String(s) => json!({ "t": "string", "v": s }),
        Value::Duration(d) => json!({ "t": "duration", "v": d.as_secs() }),
        Value::Date(Date { year, month, day }) => {
            json!({ "t": "string", "v": format!("{year}-{month:02}-{day:02}") })
        }
        Value::Fault(f) => {
            let (state, info) = match f {
                Fault::Ok => return json!({ "t": "fault_ok" }),
                Fault::Active(i) => ("active", i),
                Fault::Stored(i) => ("stored", i),
            };
            let (count, hours) = match info {
                Some(FaultInfo { count, operating_hours }) => (json!(count), json!(operating_hours)),
                None => (Json::Null, Json::Null),
            };
            json!({ "t": "fault", "state": state, "count": count, "hours": hours })
        }
    }
}

fn describe(dev: &Dev<'_>) -> Json {
    let props: Vec<Json> = dev
        .properties()
        .iter()
        .map(|p| json!({ "id": p.id, "name": p.name, "unit": p.unit, "kind": format!("{:?}", p.kind) }))
        .collect();
    let actions: Vec<Json> = dev
        .actions()
        .iter()
        .map(|a| {
            let params = match &a.params {
                None => Json::Null,
                Some(ActionParameters::Enumeration(o)) => json!({ "type": "enum", "options": o }),
                Some(ActionParameters::Flags(o)) => json!({ "type": "flags", "options": o }),
            };
            json!({ "id": a.id, "name": a.name, "kind": format!("{:?}", a.kind), "params": params })
        })
        .collect();
    json!({
        "software_id": dev.software_id(),
        "kind": dev.kind().to_string(),
        "properties": props,
        "actions": actions,
    })
}

// Same cadence as the TUI: everything once, then Io constantly, Operation every
// 3rd round, Fault every 30th, General every 90th.
fn next_kind(n: usize) -> PropertyKind {
    match n {
        0 => PropertyKind::General,
        1 => PropertyKind::Fault,
        2 => PropertyKind::Operation,
        3 => PropertyKind::Io,
        n if n % 90 == 0 => PropertyKind::General,
        n if n % 30 == 0 => PropertyKind::Fault,
        n if n % 3 == 0 => PropertyKind::Operation,
        _ => PropertyKind::Io,
    }
}

async fn query_kind(dev: &mut Dev<'_>, kind: PropertyKind) -> Result<Map<String, Json>, String> {
    let mut out = Map::new();
    for p in dev.properties().iter().filter(|p| p.kind == kind) {
        let v = time::timeout(DEVICE_TIMEOUT, dev.query_property(p))
            .await
            .map_err(|_| "The appliance stopped responding".to_string())?
            .map_err(|e| format!("Failed to query properties: {e}"))?;
        out.insert(p.id.to_string(), val(&v));
    }
    Ok(out)
}

// Err(Ok(msg)): recoverable, shown to the user. Err(Err(msg)): connection lost.
async fn run_action(dev: &mut Dev<'_>, id: &str, param: Option<String>) -> Result<(), Result<String, String>> {
    let action = dev
        .actions()
        .iter()
        .find(|a| a.id == id)
        .ok_or_else(|| Ok(format!("Unknown action: {id}")))?;
    // Like the TUI: send the (trimmed) argument only if the action takes one.
    let param = if action.params.is_some() { param.map(|p| p.trim().to_string()) } else { None };
    match time::timeout(DEVICE_TIMEOUT, dev.trigger_action(action, param.as_deref())).await {
        Err(_) => Err(Err("The appliance stopped responding".into())),
        Ok(Err(Error::InvalidArgument)) => Err(Ok("The appliance rejected the value for this action.".into())),
        Ok(Err(Error::InvalidState)) => {
            Err(Ok("The appliance is not in a state where this action is allowed.".into()))
        }
        Ok(Err(e)) => Err(Err(format!("Failed to trigger action: {e}"))),
        Ok(Ok(())) => Ok(()),
    }
}

async fn run(app: &AppHandle, dev: &mut Dev<'_>, rx: &mut mpsc::UnboundedReceiver<Cmd>) -> Outcome {
    let _ = app.emit("connected", describe(dev));
    let mut round = 0usize;
    loop {
        loop {
            match rx.try_recv() {
                Ok(Cmd::Stop) | Err(TryRecvError::Disconnected) => return Outcome::Stop,
                Ok(Cmd::Action { id, param, reply }) => match run_action(dev, &id, param).await {
                    Ok(()) => {
                        let _ = reply.send(Ok(()));
                    }
                    Err(Ok(msg)) => {
                        let _ = reply.send(Err(msg));
                    }
                    Err(Err(msg)) => {
                        let _ = reply.send(Err(msg.clone()));
                        return Outcome::Lost(msg);
                    }
                },
                Err(TryRecvError::Empty) => break,
            }
        }
        match query_kind(dev, next_kind(round)).await {
            Ok(values) => {
                let _ = app.emit("values", Json::Object(values));
            }
            Err(msg) => return Outcome::Lost(msg),
        }
        round += 1;
        time::sleep(Duration::from_millis(5)).await; // never spin if a kind is empty
    }
}

fn worker(
    app: AppHandle,
    path: String,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
    ready: std::sync::mpsc::Sender<Result<(), String>>,
) {
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            let _ = ready.send(Err(e.to_string()));
            return;
        }
    };
    // Device trait objects are not `Send`, so everything lives on this thread.
    rt.block_on(async {
        let mut port = match serial::open(&path) {
            Ok(p) => p,
            Err(e) => {
                let _ = ready.send(Err(format!("Could not open {path}: {e}")));
                return;
            }
        };
        let _ = ready.send(Ok(()));
        loop {
            // Reconnect automatically, like the TUI.
            if let Ok(Ok(mut dev)) = time::timeout(DEVICE_TIMEOUT, device::connect(&mut port)).await {
                match run(&app, &mut dev, &mut rx).await {
                    Outcome::Stop => return,
                    Outcome::Lost(msg) => {
                        let _ = app.emit("device_lost", msg);
                    }
                }
            }
            tokio::select! {
                _ = time::sleep(DEVICE_CONNECT_INTERVAL) => {}
                cmd = rx.recv() => match cmd {
                    Some(Cmd::Action { reply, .. }) => {
                        let _ = reply.send(Err("No appliance connected".into()));
                    }
                    Some(Cmd::Stop) | None => return,
                },
            }
        }
    });
}

#[tauri::command]
fn list_ports() -> Vec<String> {
    let mut ports: Vec<String> = std::fs::read_dir("/dev")
        .map(|dir| {
            dir.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("cu.") && !n.contains("Bluetooth") && n != "cu.debug-console")
                .map(|n| format!("/dev/{n}"))
                .collect()
        })
        .unwrap_or_default();
    ports.sort();
    ports
}

// Opens the port and starts the worker. The appliance can be connected later:
// the worker emits `connected` / `device_lost` events as it comes and goes.
#[tauri::command]
async fn open_port(app: AppHandle, state: State<'_, Session>, path: String) -> Result<(), String> {
    if let Some(old) = state.0.lock().unwrap().take() {
        let _ = old.send(Cmd::Stop);
    }
    let (tx, rx) = mpsc::unbounded_channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || worker(app, path, rx, ready_tx));
    let outcome = tauri::async_runtime::spawn_blocking(move || ready_rx.recv())
        .await
        .map_err(|e| e.to_string())?;
    outcome.map_err(|_| "The connection thread stopped unexpectedly".to_string())??;
    *state.0.lock().unwrap() = Some(tx);
    Ok(())
}

#[tauri::command]
fn close_port(state: State<'_, Session>) {
    if let Some(tx) = state.0.lock().unwrap().take() {
        let _ = tx.send(Cmd::Stop);
    }
}

#[tauri::command]
async fn trigger_action(state: State<'_, Session>, id: String, param: Option<String>) -> Result<(), String> {
    let tx = state.0.lock().unwrap().clone().ok_or("Not connected")?;
    let (reply, wait) = oneshot::channel();
    tx.send(Cmd::Action { id, param, reply }).map_err(|_| "Not connected".to_string())?;
    wait.await.map_err(|_| "Connection closed".to_string())?
}

fn main() {
    tauri::Builder::default()
        .manage(Session::default())
        .invoke_handler(tauri::generate_handler![list_ports, open_port, close_port, trigger_action])
        .run(tauri::generate_context!())
        .expect("error while running FreeMDU");
}
