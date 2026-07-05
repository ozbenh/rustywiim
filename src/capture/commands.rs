//! Loads `commands.yaml` (embedded at compile time) and expands each
//! `CommandSpec` into the concrete, fully-substituted command strings to
//! actually send.

use crate::capture::format::{CommandSpec, Method};
use std::collections::HashMap;

const COMMANDS_YAML: &str = include_str!("commands.yaml");

/// Parse the embedded `commands.yaml`. Panics on malformed YAML — this file
/// is only ever hand-edited by a developer rebuilding the tool, so failing
/// loudly at startup beats silently sending the wrong command.
pub fn load_command_specs() -> Vec<CommandSpec> {
    serde_yaml::from_str(COMMANDS_YAML).expect("src/capture/commands.yaml failed to parse")
}

/// One fully-substituted command ready to send, carrying the descriptive
/// (never behavior-affecting) metadata from its originating `CommandSpec`.
pub struct ExpandedCommand {
    pub command: String,
    /// Carried through so `wiim-capture`'s main loop can dispatch
    /// `Method::Getsyslog` entries to their special two-step handler instead
    /// of the normal single-GET-and-record path.
    pub method: Method,
    pub summary: Option<String>,
    pub tag: Option<String>,
    pub operation_id: Option<String>,
}

/// Expand `specs` into the commands to actually send, plus two skip lists
/// (both purely for output transparency, never silently dropped):
/// - `skipped_unsafe` — raw `command` templates skipped because `method ==
///   Set && !safe`. Never sent, regardless of `destructive`.
/// - `skipped_not_destructive` — raw `command` templates that are `method ==
///   Set && safe == true` (so would otherwise be sent) but were skipped
///   because `destructive` is false. `wiim-capture` does not mutate device
///   state unless invoked with `--destructive`.
pub fn expand_commands(specs: &[CommandSpec], destructive: bool) -> (Vec<ExpandedCommand>, Vec<String>, Vec<String>) {
    let mut out = Vec::new();
    let mut skipped_unsafe = Vec::new();
    let mut skipped_not_destructive = Vec::new();

    for spec in specs {
        if spec.method == Method::Set {
            if !spec.safe {
                skipped_unsafe.push(spec.command.clone());
                continue;
            }
            if !destructive {
                skipped_not_destructive.push(spec.command.clone());
                continue;
            }
        }
        if !spec.params.is_empty() && !spec.value_sets.is_empty() {
            panic!(
                "src/capture/commands.yaml: '{}' has both params and value_sets, which are mutually exclusive",
                spec.command
            );
        }

        let value_maps: Vec<HashMap<String, serde_yaml::Value>> = if !spec.value_sets.is_empty() {
            spec.value_sets.clone()
        } else if !spec.params.is_empty() {
            // Independent parameters are crossed (cartesian product); value_sets
            // above is the mechanism for coordinated (non-crossed) tuples.
            cartesian_product(&spec.params)
        } else {
            vec![HashMap::new()] // plain no-argument command — exactly one output
        };

        for values in &value_maps {
            let mut command = substitute(&spec.command, values);
            if spec.urlencode {
                command = urlencode_arguments(&command);
            }
            out.push(ExpandedCommand {
                command,
                method: spec.method,
                summary: spec.summary.clone(),
                tag: spec.tag.clone(),
                operation_id: spec.operation_id.clone(),
            });
        }
    }

    (out, skipped_unsafe, skipped_not_destructive)
}

/// Percent-encodes everything after the command name's first `:` (the
/// substituted argument portion), leaving the command name itself and the
/// separating `:` untouched. A command with no `:` at all is returned
/// unchanged — there's no argument portion to encode.
fn urlencode_arguments(command: &str) -> String {
    match command.split_once(':') {
        Some((name, rest)) => format!("{name}:{}", percent_encode(rest)),
        None => command.to_string(),
    }
}

/// Minimal RFC 3986 percent-encoding: unreserved characters (`A-Za-z0-9-_.~`)
/// pass through, everything else (including UTF-8 continuation bytes) is
/// encoded as `%XX`. Hand-rolled rather than pulling in a dependency — this
/// is a handful of lines and the project already favors small hand-rolled
/// parsers (see `extract_tag`/`html_unescape` in `wiim-capture.rs`) over new
/// deps for similarly small jobs.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn cartesian_product(params: &[crate::capture::format::ParamSpec]) -> Vec<HashMap<String, serde_yaml::Value>> {
    let mut combos: Vec<HashMap<String, serde_yaml::Value>> = vec![HashMap::new()];
    for p in params {
        let mut next = Vec::with_capacity(combos.len() * p.values.len().max(1));
        for combo in &combos {
            for v in &p.values {
                let mut c = combo.clone();
                c.insert(p.name.clone(), v.clone());
                next.push(c);
            }
        }
        combos = next;
    }
    combos
}

fn value_to_string(v: &serde_yaml::Value) -> String {
    match v {
        serde_yaml::Value::String(s) => s.clone(),
        serde_yaml::Value::Number(n) => n.to_string(),
        serde_yaml::Value::Bool(b) => b.to_string(),
        serde_yaml::Value::Null => String::new(),
        other => serde_yaml::to_string(other).unwrap_or_default().trim().to_string(),
    }
}

fn substitute(template: &str, values: &HashMap<String, serde_yaml::Value>) -> String {
    let mut out = template.to_string();
    for (name, v) in values {
        out = out.replace(&format!("{{{name}}}"), &value_to_string(v));
    }
    out
}
