//! The facade's echo latency against a running server, as a program that can run on a device or
//! an emulator: `echo-bench <pairing-link-file> [variants]` (`tls`, `auto`, optionally with
//! `+type` and `+paste` as the app's proof does before it types). Prints `bench ...` lines.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use atur_mobile::{Direct, DirectListener, direct_pair};
use serde_json::{Value, json};

type Failure = Box<dyn std::error::Error>;

struct Collector(Mutex<Sender<(Instant, Value)>>);

impl DirectListener for Collector {
    fn on_message(&self, json: String) {
        if let Ok(value) = serde_json::from_str(&json) {
            let _ = self
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .send((Instant::now(), value));
        }
    }
}

fn ok(answer: &str) -> Result<Value, Failure> {
    let value: Value = serde_json::from_str(answer)?;
    value
        .get("ok")
        .cloned()
        .ok_or_else(|| format!("error answer: {answer}").into())
}

fn call(direct: &Direct, method: &str, args: &Value) -> Result<Value, Failure> {
    ok(&direct.call(method.into(), args.to_string()))
}

fn fire(direct: &Direct, method: &str, args: &Value) -> Result<(), Failure> {
    ok(&direct.fire(method.into(), args.to_string())).map(drop)
}

fn millis(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

/// Text without escape sequences.
fn plain(ansi: &str) -> String {
    let mut out = String::new();
    let mut chars = ansi.chars();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// The rows the app would draw, kept from `snapshot` and `diff` events.
#[derive(Default)]
struct Rows(Vec<String>);

impl Rows {
    fn apply(&mut self, event: &Value) {
        match event["t"].as_str() {
            Some("snapshot") => {
                self.0 = event["ansi"]
                    .as_str()
                    .unwrap_or_default()
                    .split("\r\n")
                    .map(plain)
                    .collect();
            }
            Some("diff") => {
                for row in event["rows"].as_array().into_iter().flatten() {
                    let (Some(index), Some(text)) = (row[0].as_u64(), row[1].as_str()) else {
                        continue;
                    };
                    let index = usize::try_from(index).unwrap_or(usize::MAX);
                    if self.0.len() <= index {
                        self.0.resize(index + 1, String::new());
                    }
                    self.0[index] = plain(text);
                }
            }
            _ => {}
        }
    }

    fn has(&self, needle: &str) -> bool {
        self.0.iter().any(|row| row.contains(needle))
    }
}

/// Waits until `rows` satisfies `done`, for at most ten seconds; returns when the event that made it
/// true arrived.
fn wait(
    events: &Receiver<(Instant, Value)>,
    rows: &mut Rows,
    mut done: impl FnMut(&Rows) -> bool,
) -> Result<Instant, Failure> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (at, event) =
            events.recv_timeout(deadline.saturating_duration_since(Instant::now()))?;
        rows.apply(&event);
        if done(rows) {
            return Ok(at);
        }
    }
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn describe(samples: &[f64]) -> String {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let at =
        |fraction: f64| sorted[((sorted.len() as f64 * fraction) as usize).min(sorted.len() - 1)];
    format!(
        "n={} p50_ms={:.1} p90_ms={:.1} max_ms={:.1}",
        sorted.len(),
        at(0.5),
        at(0.9),
        sorted[sorted.len() - 1]
    )
}

fn run(variant: &str, credential: &Value) -> Result<(), Failure> {
    let mut parts = variant.split('+');
    let transport = parts.next().unwrap_or("tls");
    let flags: Vec<&str> = parts.collect();
    let mut credential = credential.clone();
    credential["transport"] = json!(if transport == "tls" { "tls" } else { "auto" });
    let (sender, events) = mpsc::channel();
    let direct = Direct::new(Arc::new(Collector(Mutex::new(sender))));
    let started = Instant::now();
    let info = ok(&direct.connect(credential.to_string()))?;
    let connect_ms = millis(started);
    let projects = call(&direct, "projects", &Value::Null)?;
    let home = projects
        .as_array()
        .and_then(|all| all.iter().find(|project| project["isHome"] == true))
        .ok_or("no Home project")?;
    let created = call(
        &direct,
        "createSession",
        &json!({ "projectId": home["id"], "cols": 80, "rows": 24 }),
    )?;
    let session = created["id"].as_str().ok_or("no session id")?.to_owned();
    let mut rows = Rows::default();
    let started = Instant::now();
    call(
        &direct,
        "attach",
        &json!({ "session": session, "cols": 80, "rows": 24 }),
    )?;
    wait(&events, &mut rows, |rows| !rows.0.is_empty())?;
    let attach_ms = millis(started);
    let mut extra = String::new();
    if flags.contains(&"type") {
        let started = Instant::now();
        fire(
            &direct,
            "input",
            &json!({ "session": session, "data": "echo e2e-$((40+2))\r" }),
        )?;
        wait(&events, &mut rows, |rows| {
            rows.0.iter().any(|row| row.trim() == "e2e-42")
        })?;
        extra = format!("{extra} type_ms={:.1}", millis(started));
    }
    if flags.contains(&"paste") {
        let started = Instant::now();
        call(
            &direct,
            "pasteImage",
            &json!({
                "session": session,
                "name": "paste-bench.png",
                "base64": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg=="
            }),
        )?;
        extra = format!("{extra} paste_ms={:.1}", millis(started));
    }
    call(&direct, "echoMode", &json!({ "mode": "never" }))?;
    fire(
        &direct,
        "input",
        &json!({ "session": session, "data": "cat >/dev/null\r" }),
    )?;
    thread::sleep(Duration::from_millis(1200));
    while let Ok((_, event)) = events.try_recv() {
        rows.apply(&event);
    }
    let mut samples = Vec::new();
    let mut typed = String::new();
    for _ in 0..30 {
        typed.push('q');
        let sent = Instant::now();
        fire(
            &direct,
            "input",
            &json!({ "session": session, "data": "q" }),
        )?;
        let arrived = wait(&events, &mut rows, |rows| rows.has(&typed))?;
        samples.push(arrived.duration_since(sent).as_secs_f64() * 1000.0);
        thread::sleep(Duration::from_millis(100));
    }
    let mut pings = Vec::new();
    for _ in 0..10 {
        pings.push(
            call(&direct, "ping", &Value::Null)?
                .as_f64()
                .unwrap_or_default(),
        );
    }
    let stats = call(&direct, "echoStats", &json!({ "session": session }))?;
    println!(
        "bench emu {variant} transport={} connect_ms={connect_ms:.1} attach_ms={attach_ms:.1}{extra} echo {} ping {} rtt_ms={}",
        stats["transport"],
        describe(&samples),
        describe(&pings),
        stats["rttMs"]
    );
    println!(
        "bench emu {variant} samples={:?} note={:?}",
        samples
            .iter()
            .map(|ms| (ms * 10.0).round() / 10.0)
            .collect::<Vec<_>>(),
        info["udpNote"]
    );
    call(&direct, "endSession", &json!({ "session": session }))?;
    direct.disconnect();
    Ok(())
}

fn main() -> Result<(), Failure> {
    let mut args = std::env::args().skip(1);
    let link = std::fs::read_to_string(
        args.next()
            .ok_or("usage: echo-bench <link-file> [variants]")?,
    )?;
    let variants = args.next().unwrap_or_else(|| "tls,auto".into());
    let credential = ok(&direct_pair(link.trim().to_owned(), "Echo bench".into()))?;
    for variant in variants.split(',').map(str::trim).filter(|v| !v.is_empty()) {
        if let Err(error) = run(variant, &credential) {
            println!("bench emu {variant} failed: {error}");
        }
    }
    Ok(())
}
