//! Events: one JSON object per line on stdout, so tests and scripts can wait on them.
//! Progress and diagnostics go to stderr. No event carries a secret.

use std::io::Write as _;

use serde_json::{Map, Value};

/// Print `{"event": <name>, "at_ms": <now>, ...fields}` as one line on stdout.
pub fn emit(name: &str, fields: Value) {
    let mut line = Map::new();
    line.insert("event".into(), Value::String(name.to_owned()));
    line.insert("at_ms".into(), Value::from(crate::rail::now_ms()));
    if let Value::Object(fields) = fields {
        line.extend(fields);
    }
    let mut stdout = std::io::stdout().lock();
    // A closed stdout ends nothing: the peer goes on, as its stores hold its state.
    let _ = writeln!(stdout, "{}", Value::Object(line));
    let _ = stdout.flush();
}

/// The name of an enum value from its `Debug` text: up to the first space, `(` or `{`.
pub fn variant(debug: &impl std::fmt::Debug) -> String {
    let text = format!("{debug:?}");
    text.split([' ', '(', '{'])
        .next()
        .unwrap_or_default()
        .to_owned()
}
