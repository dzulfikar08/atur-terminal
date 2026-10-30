//! The second backend of `atur-term browser`: it drives a Chromium-based
//! browser on this computer, such as Helium, through its remote debugging
//! port, with the same verbs and replies as the built-in browser.
//!
//! The browser keeps the user's own profile, so sites are already logged in.
//! Nothing is copied from the profile; the agent only sees what a page shows.
//! The port is only ever used on 127.0.0.1. Agents open their tabs in a window
//! of their own and may only drive those, unless `--any-tab` says otherwise.

mod client;
mod launch;
mod state;
mod verbs;
mod ws;

use std::path::Path;
use std::time::{Duration, Instant};

use muxy_app_core::browser::BrowserEngine;
use muxy_app_core::browser::automation::{Args, optional_string, string};
use serde_json::{Value, json};

use client::{Connection, Transport};
use launch::Browser;
pub(super) use launch::Config;
use state::Store;

/// How long a tab opened from another tab has to appear.
const TAB_APPEARS: Duration = Duration::from_secs(3);

/// What a command needs besides its verb and arguments.
#[derive(Debug)]
pub(super) struct Request<'a> {
    pub(super) config: Config,
    pub(super) any_tab: bool,
    /// Where the desktop app's note and the tab list are kept.
    pub(super) directory: &'a Path,
    /// What a new tab opens when no address is given.
    pub(super) home: String,
}

/// The agent's saved tabs and activity note for one browser.
struct Session<'a> {
    store: Store,
    engine: BrowserEngine,
    endpoint: &'a str,
}

impl Session<'_> {
    /// Changes the saved state and stamps the agent as active now.
    fn note(&self, change: impl FnOnce(&mut state::State)) -> Result<state::State, String> {
        self.store
            .update(self.engine, self.endpoint, change)
            .map_err(|error| format!("could not save the agent's tabs: {error}"))
    }
}

/// Carries out `verb` (`browser.open`, `browser.click`, …) in the browser.
pub(super) fn call(request: &Request, verb: &str, args: &Args) -> Result<Value, String> {
    let verb = verb.strip_prefix("browser.").unwrap_or(verb);
    let browser = launch::attach(&request.config)?;
    let endpoint = format!("127.0.0.1:{}", browser.port);
    let session = Session {
        store: Store::new(request.directory),
        engine: request.config.engine,
        endpoint: &endpoint,
    };
    let owned = session.note(|_| {})?;
    let result = match verb {
        "open" => {
            let url = optional_string(args, "url").unwrap_or(&request.home);
            let tab = open(&browser, &owned.tabs, url)?;
            session.note(|state| state.add(&tab))?;
            Ok(Value::String(tab))
        }
        "list" => list(&browser, request.any_tab, &owned, &session),
        _ => {
            let tab = string(args, "tabId")?;
            check_tab(tab)?;
            if !request.any_tab && !owned.owns(tab) {
                return Err(format!(
                    "browser tab not found {tab}; agents only drive tabs they opened (--any-tab \
                     lets them drive any tab, including yours)"
                ));
            }
            if verb == "close" {
                close(&browser, tab)?;
                session.note(|state| state.remove(tab))?;
                Ok(Value::Null)
            } else {
                page(&browser, tab, verb, args)
            }
        }
    };
    session.note(|_| {})?;
    result
}

/// A target ID is hexadecimal; anything else could change the path it goes
/// into.
fn check_tab(tab: &str) -> Result<(), String> {
    if !tab.is_empty() && tab.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        Ok(())
    } else {
        Err(format!("browser tab not found {tab}"))
    }
}

fn browser_connection(browser: &Browser) -> Result<Connection, String> {
    Connection::open(browser.port, &browser.socket_path)
}

fn page_connection(browser: &Browser, tab: &str) -> Result<Connection, String> {
    Connection::open(browser.port, &format!("/devtools/page/{tab}"))
        .map_err(|_| format!("browser tab not found {tab}"))
}

/// A page tab as the browser lists it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Target {
    pub(super) id: String,
    pub(super) title: String,
    pub(super) url: String,
    pub(super) opener: Option<String>,
}

/// The ordinary page tabs of a `Target.getTargets` reply; the browser's own
/// pages and extension pages are left out.
pub(super) fn page_targets(reply: &Value) -> Vec<Target> {
    reply
        .get("targetInfos")
        .and_then(Value::as_array)
        .map(|infos| {
            infos
                .iter()
                .filter(|info| info.get("type").and_then(Value::as_str) == Some("page"))
                .filter_map(|info| {
                    let text = |key: &str| info.get(key).and_then(Value::as_str).map(str::to_owned);
                    let url = text("url").unwrap_or_default();
                    if url.starts_with("chrome-extension://") || url.starts_with("devtools://") {
                        return None;
                    }
                    Some(Target {
                        id: text("targetId")?,
                        title: text("title").unwrap_or_default(),
                        url,
                        opener: text("openerId"),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn targets(connection: &mut Connection) -> Result<Vec<Target>, String> {
    Ok(page_targets(
        &connection.call("Target.getTargets", json!({}))?,
    ))
}

/// Opens `url` in a new tab and returns the tab's ID. The first tab of an
/// agent opens a window of its own; later ones open beside it, in that
/// window.
fn open(browser: &Browser, owned: &[String], url: &str) -> Result<String, String> {
    let mut connection = browser_connection(browser)?;
    let before = targets(&mut connection)?;
    let opener = owned
        .iter()
        .rev()
        .find(|tab| before.iter().any(|target| &target.id == *tab));
    if let Some(opener) = opener
        && let Some(tab) = open_beside(browser, &mut connection, opener, url)?
    {
        return Ok(tab);
    }
    let reply = connection.call(
        "Target.createTarget",
        json!({ "url": url, "newWindow": true }),
    )?;
    reply
        .get("targetId")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "the browser did not open a tab".to_owned())
}

/// Opens a tab from `opener`, which puts it in the opener's window. The
/// tab's ID comes back once the browser lists it, or none if it never does.
fn open_beside(
    browser: &Browser,
    connection: &mut Connection,
    opener: &str,
    url: &str,
) -> Result<Option<String>, String> {
    let before: Vec<_> = targets(connection)?.into_iter().map(|t| t.id).collect();
    let mut page = page_connection(browser, opener)?;
    verbs::evaluate(
        &mut page,
        "window.open('about:blank', '_blank', 'noopener'); return true;",
    )?;
    let deadline = Instant::now() + TAB_APPEARS;
    loop {
        let created = targets(connection)?.into_iter().find(|target| {
            !before.contains(&target.id) && target.opener.as_deref() == Some(opener)
        });
        if let Some(target) = created {
            if url != "about:blank" {
                let mut tab = page_connection(browser, &target.id)?;
                tab.call("Page.navigate", json!({ "url": url }))?;
            }
            return Ok(Some(target.id));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        connection.pause(Duration::from_millis(50));
    }
}

fn close(browser: &Browser, tab: &str) -> Result<(), String> {
    let mut connection = browser_connection(browser)?;
    connection
        .call("Target.closeTarget", json!({ "targetId": tab }))
        .map(|_| ())
}

/// The tabs as `list` prints them: the agent's own, or all with `--any-tab`.
fn list(
    browser: &Browser,
    any_tab: bool,
    owned: &state::State,
    session: &Session<'_>,
) -> Result<Value, String> {
    let mut connection = browser_connection(browser)?;
    let all = targets(&mut connection)?;
    let live: Vec<_> = all.iter().map(|target| target.id.clone()).collect();
    if owned.tabs.iter().any(|tab| !live.contains(tab)) {
        session.note(|state| state.keep_only(&live))?;
    }
    let rows: Vec<_> = all
        .iter()
        .filter(|target| any_tab || owned.owns(&target.id))
        .map(|target| {
            json!({
                "tabId": target.id,
                "title": target.title,
                "url": if target.url == "about:blank" { "" } else { target.url.as_str() },
                "profile": "Default",
                "active": is_visible(browser, &target.id),
            })
        })
        .collect();
    Ok(rows.into())
}

/// Whether the tab is the one showing in its window.
fn is_visible(browser: &Browser, tab: &str) -> bool {
    page_connection(browser, tab)
        .and_then(|mut page| verbs::evaluate(&mut page, "return document.visibilityState;"))
        .is_ok_and(|state| state == "visible")
}

fn page(browser: &Browser, tab: &str, verb: &str, args: &Args) -> Result<Value, String> {
    let mut connection = page_connection(browser, tab)?;
    verbs::run(&mut connection, verb, args)
}

/// Picks the engine: the command line's, else the environment's, else the
/// setting. Only an external one is handled here.
pub(super) fn engine_from(
    flag: Option<BrowserEngine>,
    environment: Option<&str>,
    setting: BrowserEngine,
) -> Result<BrowserEngine, String> {
    if let Some(engine) = flag {
        return Ok(engine);
    }
    match environment.map(str::trim).filter(|value| !value.is_empty()) {
        Some(text) => BrowserEngine::parse(text).ok_or_else(|| {
            format!("ATUR_TERM_BROWSER_ENGINE is {text}; use builtin, helium, chromium or chrome")
        }),
        None => Ok(setting),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_line_beats_the_environment_beats_the_setting() -> Result<(), String> {
        let helium = Some(BrowserEngine::Helium);
        assert_eq!(
            engine_from(helium, Some("chrome"), BrowserEngine::Builtin)?,
            BrowserEngine::Helium
        );
        assert_eq!(
            engine_from(None, Some("Chrome"), BrowserEngine::Helium)?,
            BrowserEngine::Chrome
        );
        assert_eq!(
            engine_from(None, Some(" "), BrowserEngine::Helium)?,
            BrowserEngine::Helium
        );
        assert_eq!(
            engine_from(None, None, BrowserEngine::Builtin)?,
            BrowserEngine::Builtin
        );
        assert!(engine_from(None, Some("firefox"), BrowserEngine::Builtin).is_err());
        Ok(())
    }

    #[test]
    fn only_ordinary_pages_are_tabs() {
        let reply = json!({ "targetInfos": [
            { "targetId": "A1", "type": "page", "title": "Docs", "url": "https://a.test/" },
            { "targetId": "B2", "type": "page", "title": "", "url": "about:blank", "openerId": "A1" },
            { "targetId": "C3", "type": "background_page", "url": "chrome-extension://x/bg.html" },
            { "targetId": "D4", "type": "page", "url": "chrome-extension://x/popup.html" },
            { "targetId": "E5", "type": "service_worker", "url": "https://a.test/sw.js" },
            { "type": "page", "url": "https://no-id.test/" }
        ]});
        let tabs = page_targets(&reply);
        assert_eq!(
            tabs.iter().map(|tab| tab.id.as_str()).collect::<Vec<_>>(),
            ["A1", "B2"]
        );
        assert_eq!(tabs[0].title, "Docs");
        assert_eq!(tabs[1].opener.as_deref(), Some("A1"));
        assert!(page_targets(&json!({})).is_empty());
    }

    #[test]
    fn tab_ids_cannot_change_the_path_they_go_into() {
        assert!(check_tab("A1B2C3D4E5F60718293A4B5C6D7E8F90").is_ok());
        for bad in ["", "../json/close/x", "a/b", "a b", "a?b"] {
            assert!(check_tab(bad).is_err(), "{bad}");
        }
    }
}
