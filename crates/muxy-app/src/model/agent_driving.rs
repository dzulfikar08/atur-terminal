//! Shows when an agent is driving an external browser through
//! `atur-term browser`, by watching the note the CLI leaves for each command.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui::Context;
use muxy_app_core::browser::{BrowserEngine, agent};

use super::AppModel;

const POLL_INTERVAL: Duration = Duration::from_secs(2);

impl AppModel {
    pub(super) fn start_agent_driving_poll(cx: &mut Context<Self>) {
        let Ok(directory) = muxy_core::dirs::muxy_dir() else {
            return;
        };
        let path = agent::state_path(&directory);
        cx.spawn(async move |model, cx| {
            loop {
                let note = {
                    let path = path.clone();
                    cx.background_executor()
                        .spawn(async move { std::fs::read_to_string(path).ok() })
                        .await
                };
                let driving = note
                    .and_then(|json| agent::active(&json, now_ms()))
                    .map(|driving| driving.engine);
                if model
                    .update(cx, |model, cx| model.set_agent_driving(driving, cx))
                    .is_err()
                {
                    break;
                }
                cx.background_executor().timer(POLL_INTERVAL).await;
            }
        })
        .detach();
    }

    fn set_agent_driving(&mut self, driving: Option<BrowserEngine>, cx: &mut Context<Self>) {
        if self.agent_driving != driving {
            self.agent_driving = driving;
            cx.notify();
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}
