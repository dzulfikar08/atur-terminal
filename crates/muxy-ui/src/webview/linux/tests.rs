//! Pages in the real engine. They need a desktop session to render in, so
//! they are run on request: `cargo test -p muxy-ui webview::linux -- --ignored`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "Tests fail immediately on fixture errors"
)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant};

use async_channel::Receiver;
use gpui::{Bounds, Keystroke, Modifiers, MouseButton, point, px, size};

use super::{NativeWebview, post};
use crate::webview::{Event, Pointer, assets::Source, rules};

const PATIENCE: Duration = Duration::from_secs(20);

/// The pages share one engine, so the tests that count what it holds, or
/// use the clipboard, run one at a time.
pub(super) fn alone() -> MutexGuard<'static, ()> {
    static ALONE: Mutex<()> = Mutex::new(());
    ALONE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The page's own script: `post` sends a request and resolves with its
/// answer, and `tell` sends one without waiting.
const SCRIPT: &str = "window.post=body=>window.webkit.messageHandlers.atur.postMessage(body);\
                      window.tell=(verb,value)=>{post({verb,value}).catch(()=>{})};";

struct Fixture {
    root: PathBuf,
    view: NativeWebview,
    events: Receiver<Event>,
    seen: Vec<String>,
}

impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        Self::with(files, "index.html", false)
    }

    fn with(files: &[(&str, &str)], entry: &str, transparent: bool) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "muxy-webview-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let content = root.join("extension");
        for (name, text) in files {
            let path = content.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let source = Source {
            owner: "test.owner".into(),
            directory: content,
            entry: entry.into(),
        };
        let (view, events) = NativeWebview::open(source, SCRIPT, transparent).unwrap();
        let fixture = Self {
            root,
            view,
            events,
            seen: Vec::new(),
        };
        fixture.place(400.0, 300.0);
        fixture.view.set_visible(true);
        fixture
    }

    fn place(&self, width: f32, height: f32) {
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(width), px(height)));
        self.view
            .sync(bounds, bounds, gpui::rgba(0x0000_00ff), 0.0, 1.0);
    }

    /// Waits for an event `matches` accepts, answering requests on the way so
    /// the page's promises settle.
    fn wait<T>(&mut self, what: &str, mut matches: impl FnMut(&Event) -> Option<T>) -> T {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let Ok(event) = self.events.try_recv() else {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for {what}; the page said {:?}",
                    self.seen
                );
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            if let Event::Message {
                id,
                generation,
                json,
            } = &event
            {
                self.seen.push(json.clone());
                self.view.reply(*id, *generation, r#"{"ok":true}"#);
            }
            if let Event::Snapshot { id, generation, .. } = &event {
                self.view.accept_snapshot(*id, *generation);
            }
            if let Some(found) = matches(&event) {
                return found;
            }
        }
    }

    /// Waits for the page to `tell` something under `verb`, and returns it.
    fn told(&mut self, verb: &str) -> String {
        self.until(&format!(r#""verb":"{verb}""#))
    }

    /// Waits for a request that contains `text`, and returns it.
    fn until(&mut self, text: &str) -> String {
        self.wait(text, |event| match event {
            Event::Message { json, .. } if json.contains(text) => Some(json.clone()),
            _ => None,
        })
    }

    fn loaded(&mut self) {
        self.wait("the page to load", |event| {
            matches!(event, Event::Loaded).then_some(())
        });
    }

    fn click(&self, x: f32, y: f32) {
        let position = point(px(x), px(y));
        let modifiers = Modifiers::default();
        for event in [
            Pointer::Moved {
                position,
                pressed: None,
                modifiers,
            },
            Pointer::Pressed {
                position,
                button: MouseButton::Left,
                modifiers,
            },
            Pointer::Released {
                position,
                button: MouseButton::Left,
                modifiers,
            },
        ] {
            self.view.pointer(event);
        }
    }

    fn press(&self, keystroke: &str) {
        let keystroke = Keystroke::parse(keystroke).unwrap();
        self.view.key(&keystroke, true, || None);
        self.view.key(&keystroke, false, || None);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn page(body: &str) -> String {
    format!("<!doctype html><html><body style=\"margin:0\">{body}</body></html>")
}

/// How many pages the GTK thread holds.
fn live() -> usize {
    let (answer, live) = mpsc::channel();
    post(move |runtime| {
        let _ = answer.send(runtime.map_or(0, |runtime| runtime.live()));
    });
    live.recv_timeout(PATIENCE).unwrap()
}

/// This process's `WebKit` processes: its pages' and its network process.
fn web_processes() -> usize {
    let own = std::process::id().to_string();
    std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read_to_string(entry.path().join("stat")).ok())
        .filter(|stat| {
            let Some((_, rest)) = stat.rsplit_once(") ") else {
                return false;
            };
            stat.contains("(WebKit") && rest.split(' ').nth(1) == Some(own.as_str())
        })
        .count()
}

#[test]
#[ignore = "needs a desktop session"]
fn a_page_gets_its_script_and_an_answer_to_each_request() {
    let _alone = alone();
    let mut fixture = Fixture::new(&[(
        "index.html",
        "<!doctype html><html><head><title>caf\u{e9}</title></head><body><script>\
         tell('document',document.characterSet+' '+document.title);\
         const loop={};loop.self=loop;\
         post(loop).catch(error=>tell('refused',String(error.message||error)));\
         post({verb:'ask',value:[1,'two']}).then(answer=>tell('answered',answer));\
         </script></body></html>",
    )]);
    assert!(fixture.told("document").contains("UTF-8 caf\u{e9}"));
    assert!(fixture.told("ask").contains(r#""value":[1,"two"]"#));
    let answered = fixture.told("answered");
    assert!(answered.contains(r#""value":{"ok":true}"#), "{answered}");
    let refused = fixture.seen.iter().find(|json| json.contains("refused"));
    assert!(
        refused.is_some_and(|json| json.contains(rules::INVALID_MESSAGE)),
        "{:?}",
        fixture.seen
    );
    assert_eq!(fixture.view.generation(), 1);
}

#[test]
#[ignore = "needs a desktop session"]
fn a_page_cannot_open_a_window_or_write_the_clipboard_on_its_own() {
    let _alone = alone();
    let mut fixture = Fixture::new(&[
        (
            "index.html",
            &page(
                "<a id=link href=\"second.html\" title=\"a tip\" \
                 style=\"position:absolute;left:20px;top:20px\">a link to drag</a>\
                 <script>ondragstart=()=>tell('dragged',1);\
                 onbeforeunload=event=>{event.preventDefault();return 'stay?'};\
                 window.print();\
                 const forged=new DataTransfer();forged.setData('text/plain','forged');\
                 dispatchEvent(new ClipboardEvent('copy',{clipboardData:forged}));\
                 navigator.clipboard.writeText('unasked')\
                 .then(()=>tell('wrote','yes'),()=>tell('wrote','no'));</script>",
            ),
        ),
        ("second.html", &page("<script>tell('second',1)</script>")),
    ]);
    assert!(fixture.told("wrote").contains("no"));
    let modifiers = Modifiers::default();
    fixture.view.pointer(Pointer::Pressed {
        position: point(px(40.0), px(28.0)),
        button: MouseButton::Left,
        modifiers,
    });
    for step in 1..=6_u8 {
        fixture.view.pointer(Pointer::Moved {
            position: point(
                px(40.0 + f32::from(step) * 20.0),
                px(28.0 + f32::from(step) * 10.0),
            ),
            pressed: Some(MouseButton::Left),
            modifiers,
        });
    }
    fixture.view.pointer(Pointer::Released {
        position: point(px(160.0), px(88.0)),
        button: MouseButton::Left,
        modifiers,
    });
    fixture.view.evaluate("location.href='second.html'");
    fixture.told("second");
    assert!(
        fixture
            .seen
            .iter()
            .all(|json| !json.contains("dragged") && !json.contains("forged")),
        "{:?}",
        fixture.seen
    );
    assert!(
        fixture
            .events
            .try_recv()
            .map_or(true, |event| !matches!(event, Event::Copied(_))),
        "the page copied without being asked"
    );
}

#[test]
#[ignore = "needs a desktop session"]
fn a_frame_cannot_use_the_page_s_bridge() {
    let _alone = alone();
    let frame = "<script>window.webkit.messageHandlers.atur.postMessage({verb:'forged'})\
                 .then(()=>parent.postMessage('answered','*'),\
                 error=>parent.postMessage(String(error.message||error),'*'))</script>";
    let mut fixture = Fixture::new(&[(
        "index.html",
        &page(&format!(
            "<script>addEventListener('message',event=>tell('frame',event.data));</script>\
             <iframe sandbox=\"allow-scripts\" srcdoc=\"{frame}\"></iframe>"
        )),
    )]);
    let heard = fixture.told("frame");
    assert!(heard.contains(rules::NOT_MAIN_DOCUMENT), "{heard}");
    assert!(
        fixture.seen.iter().all(|json| !json.contains("forged")),
        "{:?}",
        fixture.seen
    );
}

#[test]
#[ignore = "needs a desktop session"]
fn assets_come_from_the_extension_folder_and_nowhere_else() {
    let _alone = alone();
    let script = "const read=url=>fetch(url).then(r=>r.ok?r.text():'status '+r.status,()=>'refused');\
                  (async()=>{const results={};\
                  for(const [name,url] of Object.entries({\
                  inside:'data/a%20b.txt',unicode:'data/caf%C3%A9.txt',above:'../secret.txt',\
                  encoded:'%2e%2e/secret.txt',link:'link.txt',folder:'data',\
                  other:'muxy-ext://other.owner/index.html',file:'file:///etc/hostname'}))\
                  results[name]=await read(url);tell('assets',results);})();";
    let mut fixture = Fixture::new(&[
        ("index.html", &page(&format!("<script>{script}</script>"))),
        ("data/a b.txt", "spaced"),
        ("data/café.txt", "accented"),
    ]);
    std::fs::write(fixture.root.join("secret.txt"), "secret").unwrap();
    std::os::unix::fs::symlink(
        fixture.root.join("secret.txt"),
        fixture.root.join("extension/link.txt"),
    )
    .unwrap();
    let results = fixture.told("assets");
    assert!(results.contains(r#""inside":"spaced""#), "{results}");
    assert!(results.contains(r#""unicode":"accented""#), "{results}");
    for name in ["above", "encoded", "link", "folder", "other", "file"] {
        assert!(
            results.contains(&format!(r#""{name}":"refused""#)),
            "{results}"
        );
    }
}

#[test]
#[ignore = "needs a desktop session"]
fn a_page_stays_on_its_own_pages() {
    let _alone = alone();
    let mut fixture = Fixture::new(&[
        (
            "index.html",
            &page(
                "<iframe id=frame src=\"https://example.com/\"></iframe>\
                 <script>tell('opened',String(window.open('https://example.com/')));\
                 location.href='https://example.com/';\
                 setTimeout(()=>{tell('still',location.href);location.href='second.html'},300);\
                 </script>",
            ),
        ),
        (
            "second.html",
            &page("<script>tell('second',location.href)</script>"),
        ),
    ]);
    assert!(fixture.told("opened").contains("null"));
    assert!(
        fixture
            .told("still")
            .contains("muxy-ext://test.owner/index.html")
    );
    assert!(
        fixture
            .told("second")
            .contains("muxy-ext://test.owner/second.html")
    );
    assert_eq!(fixture.view.generation(), 2);
}

#[test]
#[ignore = "needs a desktop session"]
fn a_new_document_voids_the_requests_of_the_last() {
    let _alone = alone();
    let mut fixture = Fixture::new(&[
        (
            "index.html",
            &page(
                "<script>post({verb:'pending'});setTimeout(()=>location.href='next.html',100)</script>",
            ),
        ),
        ("next.html", &page("<script>tell('next',1)</script>")),
    ]);
    let (id, generation) = loop {
        let event = fixture.events.recv_blocking().unwrap();
        if let Event::Message { id, generation, .. } = event {
            break (id, generation);
        }
    };
    fixture.told("next");
    assert_eq!(generation + 1, fixture.view.generation());
    fixture.view.reply(id, generation, r#"{"late":true}"#);
    fixture.view.evaluate("tell('alive',1)");
    fixture.told("alive");
}

#[test]
#[ignore = "needs a desktop session"]
fn a_missing_page_is_reported_and_not_shown() {
    let _alone = alone();
    let mut fixture = Fixture::with(&[("index.html", "")], "absent.html", false);
    let failed = fixture.wait("the failure", |event| match event {
        Event::Failed(reason) => Some(reason.clone()),
        Event::Loaded => Some("loaded".into()),
        _ => None,
    });
    assert_ne!(failed, "loaded");
}

#[test]
#[ignore = "needs a desktop session"]
fn the_pointer_and_the_keyboard_reach_the_page() {
    let _alone = alone();
    let mut fixture = Fixture::new(&[(
        "index.html",
        &page(
            "<input id=field style=\"position:absolute;left:20px;top:20px;width:200px;height:30px\">\
             <button id=button style=\"position:absolute;left:20px;top:80px;width:120px;height:40px\">\
             Go</button><div style=\"height:3000px\"></div>\
             <script>button.onclick=()=>tell('clicked',1);\
             field.oninput=()=>tell('typed',field.value);field.onfocus=()=>tell('field',1);\
             field.onpaste=event=>tell('pasted',event.clipboardData.getData('text/plain'));\
             button.ondblclick=()=>navigator.clipboard.writeText('written by the page');\
             onkeydown=event=>{if(event.key==='a'&&event.ctrlKey)tell('chord',event.code)};\
             onscroll=()=>{if(scrollY>50)tell('scrolled',1)};\
             onfocus=()=>tell('focused',document.hasFocus());</script>",
        ),
    )]);
    fixture.loaded();
    fixture.view.focus();
    assert!(fixture.told("focused").contains("true"));
    fixture.click(60.0, 100.0);
    fixture.told("clicked");
    fixture.click(100.0, 35.0);
    fixture.told("field");
    fixture.press("h");
    fixture.press("shift-i");
    let shifted = Keystroke {
        key_char: Some("!".into()),
        ..Keystroke::parse("shift-1").unwrap()
    };
    fixture.view.key(&shifted, true, || None);
    fixture.until(r#""value":"hI!""#);
    fixture.press("ctrl-a");
    assert!(fixture.told("chord").contains("KeyA"));
    let paste = Keystroke::parse("ctrl-v").unwrap();
    fixture
        .view
        .key(&paste, true, || Some(r#"from "the" clip\board"#.into()));
    assert!(
        fixture
            .told("pasted")
            .contains(r#"from \"the\" clip\\board"#)
    );
    fixture.press("x");
    fixture.until(r#"clip\\boardx""#);
    fixture.press("ctrl-z");
    fixture.until(r#""value":"hI!""#);
    fixture.press("ctrl-shift-z");
    fixture.until(r#"clip\\boardx""#);
    fixture.press("ctrl-a");
    fixture.press("ctrl-c");
    let copied = fixture.wait("the copied text", |event| match event {
        Event::Copied(text) => Some(text.clone()),
        _ => None,
    });
    assert_eq!(copied, r#"from "the" clip\boardx"#);
    fixture.view.pointer(Pointer::Scrolled {
        position: point(px(200.0), px(200.0)),
        delta: point(px(0.0), px(-240.0)),
        modifiers: Modifiers::default(),
    });
    fixture.told("scrolled");
}

#[test]
#[ignore = "needs a desktop session"]
fn escape_closes_a_modal_page_instead_of_reaching_it() {
    let _alone = alone();
    let mut fixture = Fixture::new(&[(
        "index.html",
        &page("<script>onkeydown=event=>tell('key',event.key)</script>"),
    )]);
    fixture.loaded();
    fixture.view.focus();
    fixture.view.set_shortcuts(true, Vec::new());
    fixture.press("escape");
    fixture.wait("Escape", |event| {
        matches!(event, Event::Escape).then_some(())
    });
    fixture.view.set_shortcuts(false, Vec::new());
    fixture.press("escape");
    assert!(fixture.told("key").contains("Escape"));
}

#[test]
#[ignore = "needs a desktop session"]
fn a_page_is_drawn_at_its_size_and_keeps_its_transparency() {
    let _alone = alone();
    let colored = "<div style=\"position:absolute;left:0;top:0;width:100px;height:100px;\
                   background:rgb(255,0,0)\"></div>";
    let mut fixture = Fixture::with(
        &[(
            "index.html",
            &format!(
                "<!doctype html><html><body style=\"margin:0;background:transparent\">{colored}</body></html>"
            ),
        )],
        "index.html",
        true,
    );
    fixture.loaded();
    fixture.view.snapshot();
    let (width, height, corner, clear) = fixture.wait("a frame of the page", |event| match event {
        Event::Snapshot { image, .. } => {
            let pixel = |x: usize, y: usize| {
                let start = (y * image.width as usize + x) * 4;
                image.bgra[start..start + 4].to_vec()
            };
            (image.width >= 400 && image.height >= 300 && pixel(50, 50)[2] == 255)
                .then(|| (image.width, image.height, pixel(50, 50), pixel(300, 200)))
        }
        _ => None,
    });
    assert_eq!((width, height), (400, 300));
    assert_eq!(corner, [0, 0, 255, 255]);
    assert_eq!(clear, [0, 0, 0, 0]);
    fixture.place(200.0, 150.0);
    fixture.wait("a frame at the new size", |event| match event {
        Event::Snapshot { image, .. } => (image.width == 200 && image.height == 150).then_some(()),
        _ => None,
    });
    fixture.view.evaluate("tell('width',innerWidth)");
    assert!(fixture.told("width").contains(r#""value":200"#));
    std::thread::sleep(Duration::from_millis(300));
    while let Ok(event) = fixture.events.try_recv() {
        if let Event::Snapshot { id, generation, .. } = event {
            fixture.view.accept_snapshot(id, generation);
        }
    }
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        fixture.events.try_recv().is_err(),
        "a page that does not change sent a frame"
    );
}

#[test]
#[ignore = "needs a desktop session"]
fn a_hidden_page_stops_drawing() {
    let _alone = alone();
    let mut fixture = Fixture::new(&[(
        "index.html",
        &page(
            "<div style=\"width:50px;height:50px;background:red;\
             animation:spin 1s linear infinite\"></div>\
             <style>@keyframes spin{to{transform:rotate(360deg)}}</style>\
             <script>document.onvisibilitychange=()=>tell('visibility',document.visibilityState)\
             </script>",
        ),
    )]);
    fixture.loaded();
    fixture.wait("an animated frame", |event| {
        matches!(event, Event::Snapshot { .. }).then_some(())
    });
    fixture.view.set_visible(false);
    assert!(fixture.told("visibility").contains("hidden"));
    std::thread::sleep(Duration::from_millis(300));
    while fixture.events.try_recv().is_ok() {}
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        fixture.events.try_recv().is_err(),
        "a hidden page sent a frame"
    );
    fixture.view.set_visible(true);
    assert!(fixture.told("visibility").contains("visible"));
}

#[test]
#[ignore = "needs a desktop session"]
fn a_select_opens_as_a_menu_for_the_app_to_show() {
    let _alone = alone();
    let mut fixture = Fixture::new(&[(
        "index.html",
        &page(
            "<select id=choice style=\"position:absolute;left:10px;top:10px;width:200px;height:30px\">\
             <option>one<option>two<option disabled>three</select>\
             <script>choice.onchange=()=>tell('chose',choice.value)</script>",
        ),
    )]);
    fixture.loaded();
    fixture.view.focus();
    fixture.click(100.0, 25.0);
    let menu = fixture.wait("the menu", |event| match event {
        Event::Menu(menu) => menu.clone(),
        _ => None,
    });
    let labels: Vec<_> = menu
        .options
        .iter()
        .map(|option| (option.label.as_str(), option.enabled, option.selected))
        .collect();
    assert_eq!(
        labels,
        [
            ("one", true, true),
            ("two", true, false),
            ("three", false, false)
        ]
    );
    assert_eq!(menu.anchor.origin, point(px(10.0), px(10.0)));
    fixture.view.choose(Some(1));
    assert!(fixture.told("chose").contains("two"));
}

#[test]
#[ignore = "needs a desktop session"]
fn closing_pages_leaves_no_page_or_process_behind() {
    let _alone = alone();
    for _ in 0..3 {
        let before = live();
        let mut fixtures: Vec<_> = (0..3)
            .map(|_| Fixture::new(&[("index.html", &page("<script>tell('up',1)</script>"))]))
            .collect();
        for fixture in &mut fixtures {
            fixture.told("up");
            fixture.view.set_visible(false);
            fixture.view.set_visible(true);
        }
        assert_eq!(live(), before + 3);
    }
    let deadline = Instant::now() + PATIENCE;
    while (live(), web_processes()) != (0, 0) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!((live(), web_processes()), (0, 0));
}

#[test]
#[ignore = "needs a desktop session"]
fn an_answer_given_after_a_long_wait_reaches_the_page() {
    let _alone = alone();
    let mut fixture = Fixture::new(&[(
        "index.html",
        &page(
            "<script>post({verb:'ask'}).then(reply=>tell('answer',JSON.stringify(reply)));\
             for(let i=0;i<5;i++)tell('meanwhile',i)</script>",
        ),
    )]);
    let deadline = Instant::now() + PATIENCE;
    let (id, generation) = loop {
        assert!(Instant::now() < deadline, "the page never asked");
        match fixture.events.try_recv() {
            Ok(Event::Message {
                id,
                generation,
                json,
            }) if json.contains(r#""ask""#) => break (id, generation),
            Ok(Event::Snapshot { id, generation, .. }) => {
                fixture.view.accept_snapshot(id, generation);
            }
            _ => std::thread::sleep(Duration::from_millis(5)),
        }
    };
    // Answered as a permission is: after requests that came later, while the
    // page is hidden behind the prompt, and once someone made up their mind,
    // which is longer than WebKit keeps the context it read the request in.
    fixture.until(r#""value":4"#);
    fixture.view.set_visible(false);
    std::thread::sleep(Duration::from_secs(11));
    fixture
        .view
        .reply(id, generation, r#"{"ok":false,"error":"no, thanks"}"#);
    fixture.view.set_visible(true);
    assert!(fixture.told("answer").contains("no, thanks"));
}

#[test]
#[ignore = "needs a desktop session"]
fn what_a_page_stores_outlasts_the_engine_it_was_stored_in() {
    let _alone = alone();
    let data = std::env::temp_dir().join(format!("muxy-webview-data-{}", std::process::id()));
    super::set_data_directory(&data);
    let gone = || {
        let deadline = Instant::now() + PATIENCE;
        while (live(), web_processes()) != (0, 0) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        (live(), web_processes()) == (0, 0)
    };
    assert!(gone());
    Fixture::new(&[(
        "index.html",
        &page("<script>localStorage.setItem('kept','yes');tell('stored',1)</script>"),
    )])
    .told("stored");
    assert!(gone());
    let mut fixture = Fixture::new(&[(
        "index.html",
        &page("<script>tell('kept',localStorage.getItem('kept'))</script>"),
    )]);
    assert!(fixture.told("kept").contains("\"yes\""));
    drop(fixture);
    let _ = std::fs::remove_dir_all(data);
}

#[test]
fn text_is_quoted_for_a_script_without_ending_its_string() {
    use super::page::quoted;
    assert_eq!(quoted("a\"b\\c"), r#""a\"b\\c""#);
    assert_eq!(quoted("line\nbreak\r\t"), r#""line\nbreak\r\t""#);
    assert_eq!(quoted("\u{2028}\u{1}é"), "\"\\u2028\\u0001é\"");
}

#[test]
fn a_paste_is_ctrl_v_or_shift_insert_and_nothing_else() {
    let pastes = |keystroke: &str| super::pastes(&Keystroke::parse(keystroke).unwrap());
    assert!(pastes("ctrl-v"));
    assert!(pastes("shift-insert"));
    for other in ["v", "ctrl-shift-v", "ctrl-alt-v", "insert", "ctrl-c"] {
        assert!(!pastes(other), "{other}");
    }
}
