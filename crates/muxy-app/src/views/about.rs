use gpui::Context;
use muxy_core::release::Channel;
use muxy_ui::tr;

use crate::model::AppModel;

/// The dialog title.
pub(crate) fn title() -> String {
    tr!("About %@", Channel::current().app_name()).to_string()
}

/// The dialog text: the version, what the product is part of, and the credits for
/// the software it is based on and runs on.
pub(crate) fn message(version: &str) -> String {
    [
        tr!("Version %@", version).to_string(),
        tr!("Atur Terminal is part of Atur by Joyo Digitama.").to_string(),
        tr!(
            "Based on Muxy (https://github.com/muxy-app/muxy), MIT License, Copyright (c) 2026 Muxy."
        )
        .to_string(),
        tr!(
            "Terminal sessions can run on herdr (https://github.com/herdrdev/herdr), Apache-2.0, used as an external runtime; agent detection manifests adapted from herdr."
        )
        .to_string(),
        tr!("Modifications Copyright (c) 2026 Joyo Digitama.").to_string(),
    ]
    .join("\n\n")
}

impl AppModel {
    pub(crate) fn show_about(&mut self, cx: &mut Context<Self>) {
        let _ = self.window.update(cx, |_, window, cx| {
            let response = window.prompt(
                gpui::PromptLevel::Info,
                &title(),
                Some(message(env!("CARGO_PKG_VERSION")).as_str()),
                &[tr!("OK").as_ref()],
                cx,
            );
            cx.spawn(async move |_| {
                let _ = response.await;
            })
            .detach();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credits_name_the_projects_atur_terminal_builds_on() {
        let message = message("2.2.0");
        assert!(message.starts_with("Version 2.2.0"));
        assert!(message.contains("Atur by Joyo Digitama"));
        assert!(message.contains("Muxy (https://github.com/muxy-app/muxy), MIT License"));
        assert!(message.contains("herdr (https://github.com/herdrdev/herdr), Apache-2.0"));
        assert!(message.contains("Modifications Copyright (c) 2026 Joyo Digitama."));
    }
}
