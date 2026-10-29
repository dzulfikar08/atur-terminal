use super::{Category, Change, PickerKind, SettingsEvent, SettingsView};
use gpui::{AnyElement, Context, IntoElement};
use muxy_app_core::composer::submission::ImageSubmissionStrategy;
use muxy_core::capability::Feature;
use muxy_ui::controls::{self, Choice};
use muxy_ui::l10n::tr_key;
use muxy_ui::tr;

pub(super) fn rows(view: &SettingsView, cx: &mut Context<SettingsView>) -> Vec<AnyElement> {
    let settings = &view.snapshot.settings.composer;
    let voice = view.snapshot.capabilities.unsupported(Feature::Voice);
    let mut rows = Vec::new();
    for (id, label, value) in [
        (
            "voice-auto-send",
            tr_key!("Send after recording"),
            settings.voice_auto_send,
        ),
        (
            "composer-clear-after",
            tr_key!("Clear after sending"),
            settings.clear_after_sending,
        ),
        (
            "composer-clear-close",
            tr_key!("Clear on close"),
            settings.clear_on_close,
        ),
    ] {
        if !view.matches(Category::Composer, label) {
            continue;
        }
        if id == "voice-auto-send"
            && let Some(reason) = &voice
        {
            let stored = if value { tr!("On") } else { tr!("Off") };
            rows.push(view.row(id, label, view.stored(id, &stored)));
            rows.push(view.note(reason, false).into_any_element());
            continue;
        }
        rows.push(view.row(
            id,
            label,
            view.toggle(id, value, Change::Composer(id, !value), cx),
        ));
    }
    for (id, label) in [
        ("composer-font", tr_key!("Composer font")),
        ("composer-line-height", tr_key!("Composer line height")),
    ] {
        if view.matches(Category::Composer, label) {
            rows.push(view.row(id, label, view.field(id)));
        }
    }
    rows.extend(image_rows(view, cx));
    rows.extend(language_rows(view, cx));
    if muxy_ui::voice::LOCAL_MODEL
        && view.matches(Category::Composer, tr_key!("Offline voice model"))
    {
        rows.push(view.row(
            "composer-voice-model",
            tr_key!("Offline voice model"),
            view.field("composer-voice-model"),
        ));
        rows.push(view.note(&tr!("Install whisper-cli and parec, then choose a multilingual GGML model file. Leave blank to use ggml-base.bin in your user data folder under atur-terminal/voice. That folder must also contain ggml-silero-v6.2.0.bin for speech detection. Audio stays on this computer and is transcribed after you finish."), false).into_any_element());
    }
    rows
}

fn image_rows(view: &SettingsView, cx: &mut Context<SettingsView>) -> Vec<AnyElement> {
    let settings = &view.snapshot.settings.composer;
    let clipboard = view
        .snapshot
        .capabilities
        .unsupported(Feature::ClipboardImages);
    let mut rows = Vec::new();
    if view.matches(Category::Composer, tr_key!("Image submission")) {
        rows.push(view.row(
            "composer-images",
            tr_key!("Image submission"),
            controls::segmented(
                view.style(),
                "composer-images",
                &[
                    Choice {
                        enabled: clipboard.is_none(),
                        ..Choice::new("clipboard", tr!("Clipboard paste"))
                    },
                    Choice::new("path", tr!("Inline file path")),
                ],
                if settings.image_strategy == ImageSubmissionStrategy::Clipboard {
                    "clipboard"
                } else {
                    "path"
                },
                cx.listener(|_, selected: &gpui::SharedString, _, cx| {
                    cx.emit(SettingsEvent::Change(Change::Composer(
                        "composer-images",
                        selected.as_ref() == "clipboard",
                    )));
                }),
            ),
        ));
        if let Some(reason) = &clipboard {
            rows.push(
                view.note(
                    &tr!("%@ Images are sent as inline file paths.", reason.as_ref()),
                    false,
                )
                .into_any_element(),
            );
        }
    }
    rows
}

fn language_rows(view: &SettingsView, cx: &mut Context<SettingsView>) -> Vec<AnyElement> {
    let settings = &view.snapshot.settings.composer;
    let voice = view.snapshot.capabilities.unsupported(Feature::Voice);
    let mut rows = Vec::new();
    if view.matches(Category::Composer, tr_key!("Dictation language")) {
        let language = if settings.language.is_empty() {
            tr!("System language")
        } else {
            settings.language.clone().into()
        };
        let control = match &voice {
            Some(_) => view.stored("composer-language", &language),
            None => view.picker(PickerKind::Language, &language, cx),
        };
        rows.push(view.row("composer-language", tr_key!("Dictation language"), control));
        if let Some(reason) = &voice {
            rows.push(view.note(reason, false).into_any_element());
        }
    }
    rows
}
