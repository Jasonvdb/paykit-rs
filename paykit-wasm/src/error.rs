//! The engine's errors, as JavaScript sees them: an `Error` whose message is `<code>: <text>`.
//!
//! The text names the step that failed and, for an error from the SDK, the signer or the
//! store, that error's own text. It never holds a secret: the identity secret is only ever
//! read into the vault, and no error quotes a start option's value.

use std::fmt;

use wasm_bindgen::JsError;

/// A failed engine call.
#[derive(Debug)]
pub struct EngineError {
    code: &'static str,
    text: String,
}

impl EngineError {
    fn new(code: &'static str, text: impl Into<String>) -> Self {
        Self {
            code,
            text: text.into(),
        }
    }

    /// `bad_options`: a start option is missing or malformed.
    pub fn bad_options(text: impl Into<String>) -> Self {
        Self::new("bad_options", text)
    }

    /// `already_started`: `start` while this worker runs an engine.
    pub fn already_started() -> Self {
        Self::new(
            "already_started",
            "this worker runs an engine already: stop it first",
        )
    }

    /// `not_started`: a call that needs an engine before `start` or after `stop`.
    pub fn not_started() -> Self {
        Self::new("not_started", "no engine runs in this worker: start one")
    }

    /// `busy`: a call while another engine call is in progress.
    pub fn busy() -> Self {
        Self::new(
            "busy",
            "another engine call is in progress: wait for it to finish",
        )
    }

    /// `store`: a store could not be opened, read or written.
    pub fn store(step: &str, error: &dyn fmt::Display) -> Self {
        Self::new("store", format!("{step}: {error}"))
    }

    /// `identity_mismatch`: the stores hold another identity's engine.
    pub fn identity_mismatch() -> Self {
        Self::new(
            "identity_mismatch",
            "the stores hold the engine of another identity: use another store prefix",
        )
    }

    /// `signer`: the in-process signer failed or stopped short of signing.
    pub fn signer(step: &str, error: &dyn fmt::Display) -> Self {
        Self::new("signer", format!("{step}: {error}"))
    }

    /// `enrolment`: the app's request or its approval could not be made or checked.
    pub fn enrolment(step: &str, error: &dyn fmt::Display) -> Self {
        Self::new("enrolment", format!("{step}: {error}"))
    }

    /// `paykit`: the SDK failed.
    pub fn paykit(step: &str, error: &dyn fmt::Display) -> Self {
        Self::new("paykit", format!("{step}: {error}"))
    }

    /// `invalid_input`: an argument of a call other than `start` is malformed.
    pub fn invalid_input(text: impl Into<String>) -> Self {
        Self::new("invalid_input", text)
    }

    /// `homeserver`: a host Homeserver call failed during `resetIdentity`.
    pub fn homeserver(step: &str, error: &dyn fmt::Display) -> Self {
        Self::new("homeserver", format!("{step}: {error}"))
    }

    /// The code, as JavaScript sees it before the colon.
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// The text after the code.
    pub fn text(&self) -> &str {
        &self.text
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.text)
    }
}

impl From<EngineError> for JsError {
    fn from(error: EngineError) -> Self {
        JsError::new(&error.to_string())
    }
}
