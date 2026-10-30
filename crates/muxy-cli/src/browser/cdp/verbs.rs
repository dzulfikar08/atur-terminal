//! The page verbs of `atur-term browser`, as commands to a page's debugging
//! connection. Verbs that run in the page use the same scripts as the
//! built-in browser, so both engines answer alike.

use std::time::{Duration, Instant};

use muxy_app_core::browser::automation::{
    self, Args, DEFAULT_NAVIGATION_WAIT, DEFAULT_WAIT, MAX_WAIT, POLL_INTERVAL, flag,
    optional_string, string,
};
use serde_json::{Value, json};

use super::client::Transport;

/// How long a navigation that a click just caused may take to start.
const NAVIGATION_START: Duration = Duration::from_millis(500);

/// Carries out `verb` (`browser.click`, `browser.cookies.get`, …) on the
/// page behind `transport`.
pub(super) fn run(transport: &mut dyn Transport, verb: &str, args: &Args) -> Result<Value, String> {
    let verb = verb.strip_prefix("browser.").unwrap_or(verb);
    match verb {
        "navigate" => navigate(transport, string(args, "url")?),
        "reload" => transport
            .call("Page.reload", json!({}))
            .map(|_| Value::Null),
        "back" => go(transport, -1),
        "forward" => go(transport, 1),
        "screenshot" => screenshot(transport),
        "cookies.get" | "cookies.set" | "cookies.delete" | "cookies.clear" => {
            cookies(transport, verb, args)
        }
        "wait" | "waitFor" => {
            let condition = automation::condition(verb, args, |body| compiles(transport, body))?;
            wait_until(
                transport,
                &condition,
                automation::timeout(args, DEFAULT_WAIT),
            )
        }
        "waitForNavigation" => wait_for_navigation(
            transport,
            automation::timeout(args, DEFAULT_NAVIGATION_WAIT),
        ),
        _ => {
            let script = automation::script(verb, args, |body| compiles(transport, body))?
                .ok_or_else(|| format!("unknown command browser.{verb}"))?;
            evaluate(transport, &script)
        }
    }
}

/// The body of an async function as one expression.
fn wrap(body: &str) -> String {
    format!("(async () => {{\n{body}\n}})")
}

/// Whether `body` parses as the body of an async function, without running
/// it.
fn compiles(transport: &mut dyn Transport, body: &str) -> bool {
    let params = json!({
        "expression": wrap(body),
        "sourceURL": "",
        "persistScript": false,
    });
    matches!(
        transport.call("Runtime.compileScript", params),
        Ok(reply) if reply.get("exceptionDetails").is_none()
    )
}

/// The parameters that run `body` as an async function in the page and
/// return its value.
pub(super) fn evaluate_params(body: &str) -> Value {
    json!({
        "expression": format!("{}()", wrap(body)),
        "awaitPromise": true,
        "returnByValue": true,
        "userGesture": true,
    })
}

/// Runs `body` in the page and returns what it returned as JSON.
pub(super) fn evaluate(transport: &mut dyn Transport, body: &str) -> Result<Value, String> {
    let mut reply = transport.call("Runtime.evaluate", evaluate_params(body))?;
    if let Some(details) = reply.get("exceptionDetails") {
        return Err(exception_text(details));
    }
    Ok(reply
        .get_mut("result")
        .and_then(|result| result.get_mut("value"))
        .map_or(Value::Null, Value::take))
}

/// The first line of what a page script threw.
fn exception_text(details: &Value) -> String {
    let text = details
        .pointer("/exception/description")
        .and_then(Value::as_str)
        .or_else(|| details.get("text").and_then(Value::as_str))
        .unwrap_or("the page script failed");
    text.lines().next().unwrap_or(text).to_owned()
}

fn navigate(transport: &mut dyn Transport, url: &str) -> Result<Value, String> {
    let reply = transport.call("Page.navigate", json!({ "url": url }))?;
    match reply.get("errorText").and_then(Value::as_str) {
        Some(error) if !error.is_empty() => Err(format!("could not load {url}: {error}")),
        _ => Ok(Value::Null),
    }
}

/// Moves `step` entries through the history; at its end nothing happens.
fn go(transport: &mut dyn Transport, step: i64) -> Result<Value, String> {
    let history = transport.call("Page.getNavigationHistory", json!({}))?;
    let current = history.get("currentIndex").and_then(Value::as_i64);
    let entry = current
        .and_then(|current| usize::try_from(current + step).ok())
        .and_then(|index| history.get("entries")?.get(index)?.get("id")?.as_i64());
    if let Some(id) = entry {
        transport.call("Page.navigateToHistoryEntry", json!({ "entryId": id }))?;
    }
    Ok(Value::Null)
}

fn screenshot(transport: &mut dyn Transport) -> Result<Value, String> {
    let reply = transport.call("Page.captureScreenshot", json!({ "format": "png" }))?;
    let png = reply
        .get("data")
        .and_then(Value::as_str)
        .ok_or("the browser returned no screenshot")?;
    Ok(json!({ "png": png }))
}

/// A page script that returns the page's address.
const LOCATION: &str = "return location.href;";

fn cookies(transport: &mut dyn Transport, verb: &str, args: &Args) -> Result<Value, String> {
    match verb {
        "cookies.get" => {
            let url = page_url(transport, args)?;
            Ok(cookie_list(transport, &url)?
                .iter()
                .map(cookie_json)
                .collect::<Vec<_>>()
                .into())
        }
        "cookies.set" => {
            let reply = transport.call("Network.setCookie", set_cookie_params(args)?)?;
            if reply.get("success").and_then(Value::as_bool) == Some(false) {
                return Err("the browser refused the cookie".into());
            }
            Ok(Value::Null)
        }
        "cookies.delete" => {
            let url = page_url(transport, args)?;
            transport.call("Network.deleteCookies", delete_cookie_params(args, &url)?)?;
            Ok(Value::Null)
        }
        _ => {
            // Only this page's cookies: the browser holds the user's real
            // logins, and clearing them all would sign them out everywhere.
            let url = page_url(transport, args)?;
            for cookie in cookie_list(transport, &url)? {
                let name = cookie.get("name").cloned().unwrap_or_default();
                let mut params = json!({ "name": name, "url": url });
                for key in ["domain", "path"] {
                    if let Some(value) = cookie.get(key) {
                        params[key] = value.clone();
                    }
                }
                transport.call("Network.deleteCookies", params)?;
            }
            Ok(Value::Null)
        }
    }
}

/// The address a cookie command applies to: the given one, else the page's.
fn page_url(transport: &mut dyn Transport, args: &Args) -> Result<String, String> {
    if let Some(url) = optional_string(args, "url") {
        return Ok(url.to_owned());
    }
    evaluate(transport, LOCATION)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "the page has no address".to_owned())
}

fn cookie_list(transport: &mut dyn Transport, url: &str) -> Result<Vec<Value>, String> {
    let reply = transport.call("Network.getCookies", json!({ "urls": [url] }))?;
    Ok(reply
        .get("cookies")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

/// A cookie as `atur-term browser cookies get` prints it.
fn cookie_json(cookie: &Value) -> Value {
    let expires = cookie
        .get("expires")
        .and_then(Value::as_f64)
        .filter(|expires| *expires > 0.0)
        .map(|expires| expires.floor().to_string())
        .and_then(|seconds| seconds.parse::<u64>().ok());
    json!({
        "name": cookie.get("name").cloned().unwrap_or(Value::Null),
        "value": cookie.get("value").cloned().unwrap_or(Value::Null),
        "domain": cookie.get("domain").cloned().unwrap_or(Value::Null),
        "path": cookie.get("path").cloned().unwrap_or(Value::Null),
        "secure": cookie.get("secure").and_then(Value::as_bool).unwrap_or(false),
        "httpOnly": cookie.get("httpOnly").and_then(Value::as_bool).unwrap_or(false),
        "expires": expires,
    })
}

fn set_cookie_params(args: &Args) -> Result<Value, String> {
    let domain = string(args, "domain")?;
    if domain.is_empty() {
        return Err("invalid cookie domain".into());
    }
    let mut params = json!({
        "name": string(args, "name")?,
        "value": string(args, "value")?,
        "domain": domain,
        "path": optional_string(args, "path").unwrap_or("/"),
        "secure": flag(args, "secure"),
        "httpOnly": flag(args, "httpOnly"),
    });
    if let Some(expires) = args.get("expires").and_then(Value::as_u64) {
        params["expires"] = expires.into();
    }
    Ok(params)
}

fn delete_cookie_params(args: &Args, url: &str) -> Result<Value, String> {
    let mut params = json!({ "name": string(args, "name")? });
    match optional_string(args, "domain") {
        Some(domain) => params["domain"] = domain.into(),
        None => params["url"] = url.into(),
    }
    Ok(params)
}

/// Polls `condition`, a page script, until it is true or `timeout` passes.
fn wait_until(
    transport: &mut dyn Transport,
    condition: &str,
    timeout: Duration,
) -> Result<Value, String> {
    let deadline = Instant::now() + timeout.min(MAX_WAIT);
    loop {
        if evaluate(transport, condition) == Ok(Value::Bool(true)) {
            return Ok(true.into());
        }
        if Instant::now() >= deadline {
            return Ok(false.into());
        }
        transport.pause(POLL_INTERVAL);
    }
}

const LOAD_STATE: &str =
    "return { complete: document.readyState === 'complete', url: location.href };";

/// Waits for the page to settle and returns its address, or null if it
/// doesn't in time. A load that doesn't start soon counts as none, so a
/// click that navigates can be followed by this.
fn wait_for_navigation(transport: &mut dyn Transport, timeout: Duration) -> Result<Value, String> {
    transport.call("Page.enable", json!({}))?;
    let started = Instant::now();
    let deadline = started + timeout.min(MAX_WAIT);
    let mut loading = false;
    loop {
        if let Some(event) = transport.event(POLL_INTERVAL)? {
            match event.method.as_str() {
                "Page.frameStartedLoading" | "Page.frameNavigated" => loading = true,
                "Page.loadEventFired"
                | "Page.frameStoppedLoading"
                | "Page.navigatedWithinDocument"
                    if loading || event.method == "Page.navigatedWithinDocument" =>
                {
                    return location(transport);
                }
                _ => {}
            }
        }
        if !loading && started.elapsed() >= NAVIGATION_START {
            let state = evaluate(transport, LOAD_STATE)?;
            if state.get("complete").and_then(Value::as_bool) == Some(true) {
                return Ok(state.get("url").cloned().unwrap_or(Value::Null));
            }
        }
        if Instant::now() >= deadline {
            return Ok(Value::Null);
        }
    }
}

fn location(transport: &mut dyn Transport) -> Result<Value, String> {
    evaluate(transport, LOCATION)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::super::client::Event;
    use super::*;

    /// A stand-in browser: records commands and answers from a function.
    struct Fake {
        calls: Vec<(String, Value)>,
        answer: fn(&str, &Value) -> Result<Value, String>,
        events: VecDeque<Event>,
    }

    impl Fake {
        fn new(answer: fn(&str, &Value) -> Result<Value, String>) -> Self {
            Self {
                calls: Vec::new(),
                answer,
                events: VecDeque::new(),
            }
        }

        fn methods(&self) -> Vec<&str> {
            self.calls
                .iter()
                .map(|(method, _)| method.as_str())
                .collect()
        }
    }

    impl Transport for Fake {
        fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
            let reply = (self.answer)(method, &params);
            self.calls.push((method.to_owned(), params));
            reply
        }

        fn event(&mut self, _: Duration) -> Result<Option<Event>, String> {
            let event = self.events.pop_front();
            if event.is_none() {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(event)
        }

        fn pause(&mut self, _: Duration) {}
    }

    fn args(value: Value) -> Args {
        match value {
            Value::Object(map) => map,
            _ => Args::new(),
        }
    }

    fn value(value: Value) -> Result<Value, String> {
        Ok(json!({ "result": { "type": "object", "value": value } }))
    }

    fn expression(fake: &Fake, index: usize) -> String {
        fake.calls[index].1["expression"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn page_verbs_run_the_shared_script_as_an_async_function() -> Result<(), String> {
        let mut fake = Fake::new(|_, _| value(json!(true)));
        let result = run(
            &mut fake,
            "browser.click",
            &args(json!({"selector": "#go"})),
        )?;
        assert_eq!(result, json!(true));
        assert_eq!(fake.methods(), ["Runtime.evaluate"]);
        let call = &fake.calls[0].1;
        assert_eq!(call["awaitPromise"], json!(true));
        assert_eq!(call["returnByValue"], json!(true));
        let script = expression(&fake, 0);
        assert!(script.starts_with("(async () => {"), "{script}");
        assert!(script.ends_with("})()"), "{script}");
        assert!(
            script.contains(r##"document.querySelector("#go")"##),
            "{script}"
        );
        Ok(())
    }

    #[test]
    fn a_script_that_returns_nothing_is_null() -> Result<(), String> {
        let mut fake = Fake::new(|_, _| Ok(json!({ "result": { "type": "undefined" } })));
        assert_eq!(evaluate(&mut fake, "return;")?, Value::Null);
        Ok(())
    }

    #[test]
    fn a_thrown_error_is_reported_by_its_first_line() {
        let mut fake = Fake::new(|_, _| {
            Ok(json!({
                "result": {},
                "exceptionDetails": {
                    "text": "Uncaught",
                    "exception": { "description": "Error: boom\n    at <anonymous>:1:7" }
                }
            }))
        });
        assert_eq!(
            run(
                &mut fake,
                "browser.eval",
                &args(json!({"script": "throw 1"}))
            ),
            Err("Error: boom".to_owned())
        );
    }

    #[test]
    fn eval_returns_an_expression_when_it_parses_and_runs_statements_otherwise()
    -> Result<(), String> {
        let mut expression_fake = Fake::new(|method, _| {
            if method == "Runtime.compileScript" {
                Ok(json!({ "scriptId": "1" }))
            } else {
                value(json!(2))
            }
        });
        run(
            &mut expression_fake,
            "browser.eval",
            &args(json!({"script": "1 + 1;"})),
        )?;
        assert_eq!(
            expression_fake.methods(),
            ["Runtime.compileScript", "Runtime.evaluate"]
        );
        assert!(expression(&expression_fake, 1).contains("return (\n1 + 1\n);"));

        let mut statements_fake = Fake::new(|method, _| {
            if method == "Runtime.compileScript" {
                Ok(json!({ "exceptionDetails": { "text": "SyntaxError" } }))
            } else {
                value(json!(2))
            }
        });
        run(
            &mut statements_fake,
            "browser.eval",
            &args(json!({"script": "const a = 1; return a"})),
        )?;
        assert!(expression(&statements_fake, 1).contains("const a = 1; return a"));
        assert!(!expression(&statements_fake, 1).contains("return (\n"));
        Ok(())
    }

    #[test]
    fn navigation_failures_name_the_address() {
        let mut fake = Fake::new(|_, _| Ok(json!({ "errorText": "net::ERR_NAME_NOT_RESOLVED" })));
        assert_eq!(
            run(
                &mut fake,
                "browser.navigate",
                &args(json!({"url": "https://a.test"}))
            ),
            Err("could not load https://a.test: net::ERR_NAME_NOT_RESOLVED".to_owned())
        );
    }

    #[test]
    fn history_moves_stop_at_either_end() -> Result<(), String> {
        let mut fake = Fake::new(|method, _| match method {
            "Page.getNavigationHistory" => Ok(json!({
                "currentIndex": 1,
                "entries": [{ "id": 10 }, { "id": 11 }, { "id": 12 }]
            })),
            _ => Ok(json!({})),
        });
        run(&mut fake, "browser.back", &Args::new())?;
        assert_eq!(
            fake.calls[1],
            (
                "Page.navigateToHistoryEntry".to_owned(),
                json!({"entryId": 10})
            )
        );
        run(&mut fake, "browser.forward", &Args::new())?;
        assert_eq!(fake.calls[3].1, json!({"entryId": 12}));

        let mut start =
            Fake::new(|_, _| Ok(json!({ "currentIndex": 0, "entries": [{ "id": 1 }] })));
        run(&mut start, "browser.back", &Args::new())?;
        assert_eq!(start.methods(), ["Page.getNavigationHistory"]);
        Ok(())
    }

    #[test]
    fn screenshots_are_base64_png() -> Result<(), String> {
        let mut fake = Fake::new(|_, _| Ok(json!({ "data": "iVBOR" })));
        assert_eq!(
            run(&mut fake, "browser.screenshot", &Args::new())?,
            json!({"png": "iVBOR"})
        );
        assert_eq!(fake.calls[0].1, json!({"format": "png"}));
        Ok(())
    }

    #[test]
    fn cookies_are_listed_for_the_page_and_shaped_like_the_built_in_browser() -> Result<(), String>
    {
        let mut fake = Fake::new(|method, _| match method {
            "Network.getCookies" => Ok(json!({ "cookies": [
                { "name": "a", "value": "1", "domain": ".x.test", "path": "/", "secure": true,
                  "httpOnly": false, "expires": 1_900_000_000.5, "size": 2 },
                { "name": "s", "value": "2", "domain": "x.test", "path": "/", "secure": false,
                  "httpOnly": true, "expires": -1, "session": true }
            ]})),
            _ => value(json!("https://x.test/page")),
        });
        let cookies = run(&mut fake, "browser.cookies.get", &Args::new())?;
        assert_eq!(
            cookies,
            json!([
                { "name": "a", "value": "1", "domain": ".x.test", "path": "/", "secure": true,
                  "httpOnly": false, "expires": 1_900_000_000_u64 },
                { "name": "s", "value": "2", "domain": "x.test", "path": "/", "secure": false,
                  "httpOnly": true, "expires": null }
            ])
        );
        assert_eq!(fake.calls[1].1, json!({"urls": ["https://x.test/page"]}));
        Ok(())
    }

    #[test]
    fn setting_and_deleting_cookies_map_their_arguments() -> Result<(), String> {
        let mut fake = Fake::new(|method, _| match method {
            "Network.setCookie" => Ok(json!({ "success": true })),
            _ => value(json!("https://x.test/")),
        });
        run(
            &mut fake,
            "browser.cookies.set",
            &args(
                json!({"name": "id", "value": "7", "domain": "x.test", "secure": true, "expires": 99}),
            ),
        )?;
        assert_eq!(
            fake.calls[0].1,
            json!({"name": "id", "value": "7", "domain": "x.test", "path": "/",
                   "secure": true, "httpOnly": false, "expires": 99})
        );
        run(
            &mut fake,
            "browser.cookies.delete",
            &args(json!({"name": "id"})),
        )?;
        assert_eq!(
            fake.calls[2],
            (
                "Network.deleteCookies".to_owned(),
                json!({"name": "id", "url": "https://x.test/"})
            )
        );
        run(
            &mut fake,
            "browser.cookies.delete",
            &args(json!({"name": "id", "domain": "y.test", "url": "https://x.test/"})),
        )?;
        assert_eq!(fake.calls[3].1, json!({"name": "id", "domain": "y.test"}));
        Ok(())
    }

    #[test]
    fn a_refused_cookie_is_an_error() {
        let mut fake = Fake::new(|_, _| Ok(json!({ "success": false })));
        let result = run(
            &mut fake,
            "browser.cookies.set",
            &args(json!({"name": "a", "value": "b", "domain": "x.test"})),
        );
        assert_eq!(result, Err("the browser refused the cookie".to_owned()));
        assert!(
            run(
                &mut fake,
                "browser.cookies.set",
                &args(json!({"name": "a", "value": "b", "domain": ""}))
            )
            .is_err()
        );
    }

    #[test]
    fn clearing_cookies_only_touches_the_page_s_own() -> Result<(), String> {
        let mut fake = Fake::new(|method, _| match method {
            "Network.getCookies" => Ok(json!({ "cookies": [
                { "name": "a", "domain": "x.test", "path": "/" },
                { "name": "b", "domain": ".x.test", "path": "/app" }
            ]})),
            _ => value(json!("https://x.test/")),
        });
        run(&mut fake, "browser.cookies.clear", &Args::new())?;
        let methods = fake.methods();
        assert!(!methods.contains(&"Network.clearBrowserCookies"));
        assert!(!methods.contains(&"Storage.clearCookies"));
        let deletes: Vec<_> = fake
            .calls
            .iter()
            .filter(|(method, _)| method == "Network.deleteCookies")
            .map(|(_, params)| params.clone())
            .collect();
        assert_eq!(
            deletes,
            [
                json!({"name": "a", "url": "https://x.test/", "domain": "x.test", "path": "/"}),
                json!({"name": "b", "url": "https://x.test/", "domain": ".x.test", "path": "/app"}),
            ]
        );
        Ok(())
    }

    #[test]
    fn waits_report_whether_the_condition_held() -> Result<(), String> {
        let mut holds = Fake::new(|_, _| value(json!(true)));
        let found = run(
            &mut holds,
            "browser.wait",
            &args(json!({"text": "Welcome", "timeoutMs": 50})),
        )?;
        assert_eq!(found, json!(true));
        assert!(expression(&holds, 0).contains(r#".includes("Welcome")"#));

        let mut never = Fake::new(|_, _| value(json!(false)));
        let timed_out = run(
            &mut never,
            "browser.waitFor",
            &args(json!({"selector": "#late", "timeoutMs": 20})),
        )?;
        assert_eq!(timed_out, json!(false));
        assert!(!never.calls.is_empty());
        Ok(())
    }

    #[test]
    fn navigation_waits_for_the_load_a_click_started() -> Result<(), String> {
        let mut fake = Fake::new(|method, _| match method {
            "Runtime.evaluate" => value(json!("https://x.test/next")),
            _ => Ok(json!({})),
        });
        for method in ["Page.frameStartedLoading", "Page.loadEventFired"] {
            fake.events.push_back(Event {
                method: method.to_owned(),
                params: json!({}),
            });
        }
        let url = run(
            &mut fake,
            "browser.waitForNavigation",
            &args(json!({"timeoutMs": 5000})),
        )?;
        assert_eq!(url, json!("https://x.test/next"));
        assert_eq!(fake.methods()[0], "Page.enable");
        Ok(())
    }

    #[test]
    fn navigation_waits_end_with_the_settled_page_when_nothing_starts() -> Result<(), String> {
        let mut fake = Fake::new(|method, _| match method {
            "Runtime.evaluate" => value(json!({"complete": true, "url": "https://x.test/"})),
            _ => Ok(json!({})),
        });
        let url = run(
            &mut fake,
            "browser.waitForNavigation",
            &args(json!({"timeoutMs": 3000})),
        )?;
        assert_eq!(url, json!("https://x.test/"));
        Ok(())
    }

    #[test]
    fn navigation_waits_give_up_with_null() -> Result<(), String> {
        let mut fake = Fake::new(|method, _| match method {
            "Runtime.evaluate" => value(json!({"complete": false, "url": "https://x.test/"})),
            _ => Ok(json!({})),
        });
        let url = run(
            &mut fake,
            "browser.waitForNavigation",
            &args(json!({"timeoutMs": 50})),
        )?;
        assert_eq!(url, Value::Null);
        Ok(())
    }

    #[test]
    fn unknown_verbs_are_reported() {
        let mut fake = Fake::new(|_, _| Ok(json!({})));
        assert_eq!(
            run(&mut fake, "browser.dance", &Args::new()),
            Err("unknown command browser.dance".to_owned())
        );
    }

    #[test]
    fn storage_and_reads_use_the_page_scripts() -> Result<(), String> {
        let mut fake = Fake::new(|_, _| value(json!("v")));
        run(
            &mut fake,
            "browser.storage.get",
            &args(json!({"key": "k", "kind": "session"})),
        )?;
        assert!(expression(&fake, 0).contains(r#"window.sessionStorage.getItem("k")"#));
        run(&mut fake, "browser.read", &Args::new())?;
        assert!(expression(&fake, 1).contains("document.title"));
        Ok(())
    }
}
