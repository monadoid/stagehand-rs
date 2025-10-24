//! Stagehand DOM helper script embedding.
//!
//! The Rust port embeds the same `domScripts.js` bundle used by the Python
//! implementation so that page-context helpers remain in sync across
//! languages. Keeping the script in its own `.js` file allows editors to offer
//! proper syntax highlighting while still bundling it as a string at compile
//! time.

/// Embedded contents of `stagehand-python/stagehand/domScripts.js`.
pub const STAGEHAND_DOM_SCRIPT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/scripts/dom_scripts.js"
));

/// Return the embedded Stagehand DOM script.
///
/// Provided as a function to make it easy to swap-in alternate bundles in tests
/// (e.g. truncated fixtures) while the default implementation simply exposes
/// the constant string slice.
pub fn stagehand_dom_script() -> &'static str {
    STAGEHAND_DOM_SCRIPT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_script_is_non_empty() {
        assert!(!STAGEHAND_DOM_SCRIPT.trim().is_empty());
    }

    #[test]
    fn embedded_script_contains_expected_marker() {
        assert!(
            STAGEHAND_DOM_SCRIPT.contains("getScrollableElementXpaths"),
            "dom script should expose Stagehand scrollable element helper"
        );
    }
}
