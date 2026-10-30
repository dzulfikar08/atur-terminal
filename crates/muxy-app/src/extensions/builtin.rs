//! Features the app ships natively but presents beside installed extensions.

use muxy_ui::tr;

/// The built-in extension id of screen mirroring.
pub(crate) const SCREEN: &str = "screen";

/// A built-in extension as Settings → Extensions lists it.
pub(crate) struct Builtin {
    pub(crate) id: &'static str,
    pub(crate) title: String,
    pub(crate) description: String,
}

pub(crate) fn all() -> Vec<Builtin> {
    vec![Builtin {
        id: SCREEN,
        title: tr!("Screen Mirroring").to_string(),
        description: tr!("View a connected computer's desktop, shared by its server.").to_string(),
    }]
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_registered_builtin_is_listed() {
        let listed: Vec<_> = super::all().into_iter().map(|builtin| builtin.id).collect();
        assert_eq!(listed, muxy_app_core::extensions::BUILTINS);
    }
}
