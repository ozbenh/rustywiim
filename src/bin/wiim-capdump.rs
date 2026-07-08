//! `wiim-capdump` — prints a `wiim-capture` JSON output file for a human to
//! read, not as a JSON dump: one clearly delimited block per command, each
//! showing the command name, a success/failure badge, and its response
//! (JSON/XML/text/decoded-syslog) pretty-printed, with a blank-line gap
//! between commands. Three output modes:
//! - **fancy** (default when stdout is a terminal): ANSI bold/color.
//! - **plain** (default when stdout is piped/redirected): the same
//!   structure, no escape codes.
//! - **markdown** (`--markdown`, works either way): headings/code fences,
//!   for saving to a `.md` file or pasting somewhere that renders Markdown.
//!
//! Along the way it also:
//! - decodes any `"format": "base64"` blob's `body` back to text for
//!   display when the decoded bytes are valid UTF-8 (plain ASCII trivially
//!   counts); a body that doesn't decode to UTF-8 is shown as a short binary
//!   note, not dumped as a wall of base64.
//! - specifically decodes the `getsyslog:download` command's raw encrypted
//!   bytes (RC4-decrypt, then gunzip+untar if applicable — see
//!   `decode_syslog_payload`) into readable log content, purely from what
//!   `wiim-capture` already fetched and stored; no network access here.
//! The *file* is never modified — this only affects what gets printed. The
//! body-decoding helpers stay schema-agnostic (generic `serde_json::Value`
//! shape matching, not `rustywiim::capture::format` structs), but the overall
//! layout (metadata header, `commands` array, `upnp` section) does assume
//! `CaptureFile`'s current top-level shape, unlike the old plain-JSON-dump
//! version of this tool.
//!
//! Usage: `wiim-capdump [--markdown] <capture-file.json>`
//!
//! `wiim-capdump [--markdown] -` — reads from stdin instead of a file (the
//! usual Unix `-` convention), and pretty-prints the raw content as a single
//! blob (format auto-detected: JSON/XML/plain text) rather than parsing it
//! as a whole `CaptureFile`. Pairs with `wiim-capture --one <target> <ip>`,
//! which prints exactly this shape — one command/UPnP action's bare
//! response body, nothing wrapped around it.

use base64::Engine;
use std::io::{IsTerminal, Read};

/// RC4 (ARC4) key-scheduling + keystream generation — hand-rolled, no
/// dependency; this is the only place it's needed (LinkPlay's `getsyslog`
/// download encryption). Confirmed against linkplay-cli's
/// `linkplay_cli/cli.py::getsyslog`, which decrypts the same way with
/// `Crypto.Cipher.ARC4`.
fn rc4(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut s: [u8; 256] = std::array::from_fn(|i| i as u8);
    let mut j: u8 = 0;
    for i in 0..256 {
        j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
        s.swap(i, j as usize);
    }
    let mut out = Vec::with_capacity(data.len());
    let mut i: u8 = 0;
    let mut j: u8 = 0;
    for &byte in data {
        i = i.wrapping_add(1);
        j = j.wrapping_add(s[i as usize]);
        s.swap(i as usize, j as usize);
        let k = s[(s[i as usize].wrapping_add(s[j as usize])) as usize];
        out.push(byte ^ k);
    }
    out
}

const SYSLOG_KEY: &[u8] = b"wiimulogsecure\0\0";
const SYSLOG_CHUNK_SIZE: usize = 10240;

/// Each `SYSLOG_CHUNK_SIZE`-byte chunk of the download is independently RC4-
/// encrypted with a *fresh* keystream (the same key re-initialized every
/// chunk) — not one continuous stream cipher over the whole payload.
/// Confirmed against linkplay-cli's `cli.py::getsyslog`, which does exactly
/// this (`ARC4.new(config.log_key)` re-created inside the per-chunk loop).
fn decrypt_syslog(encrypted: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(encrypted.len());
    for chunk in encrypted.chunks(SYSLOG_CHUNK_SIZE) {
        out.extend(rc4(SYSLOG_KEY, chunk));
    }
    out
}

fn bytes_to_display_value(bytes: &[u8], binary_note: &str) -> serde_json::Value {
    match String::from_utf8(bytes.to_vec()) {
        Ok(text) => serde_json::Value::String(text),
        Err(_) => serde_json::Value::String(format!("<{binary_note}, {} bytes, not valid UTF-8>", bytes.len())),
    }
}

/// Decodes the getsyslog download payload: RC4-decrypt, then if the result
/// looks gzipped (`\x1f\x8b\x08` magic — LinkPlay always tars-then-gzips the
/// real log, per `cli.py::getsyslog`), gunzip + untar and return a
/// `{filename: content}` object; otherwise return the decrypted bytes
/// directly (text if valid UTF-8, else a short binary note).
fn decode_syslog_payload(encrypted: &[u8]) -> serde_json::Value {
    let decrypted = decrypt_syslog(encrypted);
    if !decrypted.starts_with(&[0x1f, 0x8b, 0x08]) {
        return bytes_to_display_value(&decrypted, "not gzipped");
    }

    let mut gz = flate2::read::GzDecoder::new(&decrypted[..]);
    let mut tar_bytes = Vec::new();
    if gz.read_to_end(&mut tar_bytes).is_err() {
        return bytes_to_display_value(&decrypted, "gunzip failed, showing raw decrypted bytes");
    }

    let mut archive = tar::Archive::new(&tar_bytes[..]);
    let entries = match archive.entries() {
        Ok(e) => e,
        Err(_) => return bytes_to_display_value(&tar_bytes, "not a tar archive, showing gunzipped bytes"),
    };

    let mut files = serde_json::Map::new();
    for entry in entries {
        let Ok(mut entry) = entry else { continue };
        let path = entry.path().map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|_| "?".to_string());
        let mut content = Vec::new();
        let _ = entry.read_to_end(&mut content);
        files.insert(path, bytes_to_display_value(&content, "binary file"));
    }
    serde_json::Value::Object(files)
}

/// Finds every `{"command": "getsyslog:download", "body": "<base64>"}`
/// object and replaces its body with the decoded log content/file listing —
/// see `decode_syslog_payload`. Runs before the generic base64/xml decode
/// pass below so that pass doesn't also try (and fail) to UTF-8-decode the
/// still-encrypted bytes.
fn decode_syslog_entries(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            let is_syslog_download =
                matches!(map.get("command"), Some(serde_json::Value::String(c)) if c == "getsyslog:download");
            if is_syslog_download {
                if let Some(serde_json::Value::String(encoded)) = map.get("body").cloned() {
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(encoded.as_bytes()) {
                        map.insert("body".to_string(), decode_syslog_payload(&bytes));
                        map.insert(
                            "format".to_string(),
                            serde_json::Value::String("syslog (RC4-decrypted, gunzip/untar if applicable)".to_string()),
                        );
                    }
                }
            }
            for val in map.values_mut() {
                decode_syslog_entries(val);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                decode_syslog_entries(item);
            }
        }
        _ => {}
    }
}

/// Colorizes one `<tag attr="value">`/`</tag>`/`<tag/>`/`<?xml ...?>` token
/// (angle brackets + structural punctuation in `JQ_STRUCT`, tag/attribute
/// names in `JQ_KEY`, attribute values in `JQ_STRING`, prolog/comments dimmed
/// in `JQ_NULL`) — a no-op outside `OutputMode::Fancy` (see `OutputMode::ansi`),
/// so Plain/Markdown output is unaffected (Markdown's ```xml fence gets its
/// own syntax highlighting from whatever renders it).
fn colorize_xml_tag(mode: OutputMode, tag: &str) -> String {
    if tag.starts_with("<?") || tag.starts_with("<!") {
        return mode.ansi(tag, JQ_NULL);
    }
    let closing = tag.starts_with("</");
    let self_closing = tag.ends_with("/>");
    let start = if closing { 2 } else { 1 };
    let end = tag.len() - if self_closing { 2 } else { 1 };
    let inner = tag.get(start..end).unwrap_or("");
    let mut parts = inner.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or("");
    let attrs = parts.next().unwrap_or("").trim();

    let mut out = String::new();
    out.push_str(&mode.ansi(if closing { "</" } else { "<" }, JQ_STRUCT));
    out.push_str(&mode.ansi(name, JQ_KEY));
    if !attrs.is_empty() {
        out.push(' ');
        out.push_str(&colorize_xml_attrs(mode, attrs));
    }
    if self_closing {
        out.push_str(&mode.ansi(" /", JQ_STRUCT));
    }
    out.push_str(&mode.ansi(">", JQ_STRUCT));
    out
}

/// Colorizes a run of `name="value"` (or `name='value'`) attribute pairs —
/// name in `JQ_KEY`, `=` in `JQ_STRUCT`, quoted value in `JQ_STRING`. Falls
/// back to printing whatever's left unstyled if the shape doesn't match
/// (malformed/unexpected attribute syntax) rather than dropping it.
fn colorize_xml_attrs(mode: OutputMode, attrs: &str) -> String {
    let mut out = String::new();
    let mut rest = attrs;
    let mut first = true;
    while let Some(eq) = rest.find('=') {
        let name = rest[..eq].trim();
        if name.is_empty() {
            break;
        }
        let after_eq = rest[eq + 1..].trim_start();
        let Some(quote) = after_eq.chars().next().filter(|c| *c == '"' || *c == '\'') else {
            break;
        };
        let Some(close_rel) = after_eq[quote.len_utf8()..].find(quote) else {
            break;
        };
        let value_end = quote.len_utf8() + close_rel + quote.len_utf8();
        let value = &after_eq[..value_end];

        if !first {
            out.push(' ');
        }
        first = false;
        out.push_str(&mode.ansi(name, JQ_KEY));
        out.push_str(&mode.ansi("=", JQ_STRUCT));
        out.push_str(&mode.ansi(value, JQ_STRING));
        rest = after_eq[value_end..].trim_start();
    }
    if !rest.is_empty() {
        if !first {
            out.push(' ');
        }
        out.push_str(rest);
    }
    out
}

/// Simple, non-CDATA-aware XML pretty-printer: indents by nesting depth and
/// colorizes tags/attributes/text (see `colorize_xml_tag`). Sufficient for
/// the well-formed, mostly-attribute-light XML this project ever captures
/// (UPnP description.xml, SOAP responses) — not a general XML formatter,
/// same spirit as `wiim-capture.rs`'s `extract_tag`.
fn pretty_print_xml(mode: OutputMode, xml: &str) -> String {
    let mut out = String::new();
    let mut depth: usize = 0;
    let bytes = xml.as_bytes();
    let len = xml.len();
    let mut i = 0usize;
    while i < len {
        if bytes[i] == b'<' {
            let end = xml[i..].find('>').map(|p| i + p + 1).unwrap_or(len);
            let tag = &xml[i..end];
            let colored = colorize_xml_tag(mode, tag);
            if tag.starts_with("</") {
                depth = depth.saturating_sub(1);
                out.push_str(&"  ".repeat(depth));
                out.push_str(&colored);
                out.push('\n');
            } else if tag.starts_with("<?") || tag.starts_with("<!") || tag.ends_with("/>") {
                out.push_str(&"  ".repeat(depth));
                out.push_str(&colored);
                out.push('\n');
            } else {
                out.push_str(&"  ".repeat(depth));
                out.push_str(&colored);
                out.push('\n');
                depth += 1;
            }
            i = end;
        } else {
            let end = xml[i..].find('<').map(|p| i + p).unwrap_or(len);
            let text = xml[i..end].trim();
            // A text node can itself contain literal newlines (either from
            // the device's own response or a `\n` escape already unescaped
            // by JSON parsing) — indent every line at the current depth
            // rather than dumping a raw, unindented multi-line blob.
            for line in text.lines() {
                let line = line.trim();
                if !line.is_empty() {
                    out.push_str(&"  ".repeat(depth));
                    out.push_str(&mode.ansi(line, JQ_STRING));
                    out.push('\n');
                }
            }
            i = end;
        }
    }
    out.trim_end().to_string()
}

/// Recursively finds every JSON object matching `{"format": "base64", "body":
/// "<string>"}` and, if the base64 decodes to valid UTF-8, replaces `body`
/// with the decoded text — purely for display, on the in-memory value the
/// caller is about to print. `xml` bodies are deliberately left alone here —
/// they're pretty-printed/colorized at render time instead (`render_body`),
/// since that's where an `OutputMode` is available to colorize with.
fn decode_bodies_for_display(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            if map.get("format").and_then(|f| f.as_str()) == Some("base64") {
                if let Some(serde_json::Value::String(encoded)) = map.get("body").cloned() {
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(encoded.as_bytes()) {
                        // "assuming it's ASCII or utf-8" — UTF-8 validity covers both;
                        // plain ASCII is always valid UTF-8.
                        if let Ok(text) = String::from_utf8(bytes) {
                            map.insert("body".to_string(), serde_json::Value::String(text));
                            map.insert(
                                "format".to_string(),
                                serde_json::Value::String("base64 (decoded for display)".to_string()),
                            );
                        }
                    }
                }
            }
            for val in map.values_mut() {
                decode_bodies_for_display(val);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                decode_bodies_for_display(item);
            }
        }
        _ => {}
    }
}

// ── Human-readable rendering ─────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum OutputMode {
    /// stdout is a terminal: ANSI bold/color.
    Fancy,
    /// stdout is piped/redirected: same structure, no escape codes.
    Plain,
    /// `--markdown` was passed: headings/code fences, regardless of whether
    /// stdout is a terminal.
    Markdown,
}

const ANSI_RESET: &str = "\x1b[0m";
const ANSI_BOLD: &str = "\x1b[1m";
const ANSI_DIM: &str = "\x1b[2m";
// Bare SGR codes — `OutputMode::ansi()` wraps these as `\x1b[{code}m...\x1b[0m`
// itself, so these must NOT include the `\x1b[`/`m` themselves (unlike
// ANSI_BOLD/ANSI_DIM below, which `bold()`/`dim()` splice in directly).
const ANSI_RED: &str = "31";
const ANSI_GREEN: &str = "32";
const ANSI_YELLOW: &str = "33";

// jq's own default JQ_COLORS (null:false:true:numbers:strings:arrays:objects:objectkeys),
// reused here so JSON fragments look the way anyone used to `jq`'s terminal
// output already expects.
const JQ_NULL: &str = "1;30";
const JQ_BOOL: &str = "0;39";
const JQ_NUMBER: &str = "0;39";
const JQ_STRING: &str = "0;32";
const JQ_STRUCT: &str = "1;39";
const JQ_KEY: &str = "34;1";

impl OutputMode {
    fn detect(markdown_flag: bool) -> Self {
        if markdown_flag {
            OutputMode::Markdown
        } else if std::io::stdout().is_terminal() {
            OutputMode::Fancy
        } else {
            OutputMode::Plain
        }
    }

    fn ansi(self, text: &str, code: &str) -> String {
        match self {
            OutputMode::Fancy => format!("\x1b[{code}m{text}{ANSI_RESET}"),
            _ => text.to_string(),
        }
    }

    fn bold(self, text: &str) -> String {
        match self {
            OutputMode::Fancy => format!("{ANSI_BOLD}{text}{ANSI_RESET}"),
            OutputMode::Markdown => format!("**{text}**"),
            OutputMode::Plain => text.to_string(),
        }
    }

    fn dim(self, text: &str) -> String {
        match self {
            OutputMode::Fancy => format!("{ANSI_DIM}{text}{ANSI_RESET}"),
            OutputMode::Markdown => format!("_{text}_"),
            OutputMode::Plain => text.to_string(),
        }
    }

    fn heading(self, level: u8, text: &str) -> String {
        match self {
            OutputMode::Markdown => format!("{} {}", "#".repeat(level as usize), text),
            _ => self.bold(text),
        }
    }

    fn rule(self) -> String {
        match self {
            OutputMode::Markdown => "---".to_string(),
            _ => "\u{2500}".repeat(70),
        }
    }

    /// Wraps `content` in a fenced code block in Markdown mode; returned as-is
    /// otherwise (Fancy/Plain already indent/color inline, no fence needed).
    fn code_block(self, lang: &str, content: &str) -> String {
        match self {
            OutputMode::Markdown => format!("```{lang}\n{content}\n```"),
            _ => content.to_string(),
        }
    }
}

/// jq-style colorized, indented JSON printer (2-space indent, object keys in
/// bold blue, strings in green, structural brackets in bold — jq's own
/// default `JQ_COLORS`). Colors are no-ops outside `OutputMode::Fancy`, so
/// this is also just the plain pretty-printer for Plain/Markdown modes — one
/// implementation, not a separate colored/uncolored pair. Keys print in
/// whatever order `serde_json::Map` iterates (alphabetical — this crate
/// doesn't enable the `preserve_order` feature), matching this tool's
/// previous plain-JSON-dump behavior.
fn write_json_colored(v: &serde_json::Value, mode: OutputMode, depth: usize, out: &mut String) {
    let indent = "  ".repeat(depth);
    let child_indent = "  ".repeat(depth + 1);
    match v {
        serde_json::Value::Null => out.push_str(&mode.ansi("null", JQ_NULL)),
        serde_json::Value::Bool(b) => out.push_str(&mode.ansi(&b.to_string(), JQ_BOOL)),
        serde_json::Value::Number(n) => out.push_str(&mode.ansi(&n.to_string(), JQ_NUMBER)),
        serde_json::Value::String(s) => {
            out.push_str(&mode.ansi(&serde_json::to_string(s).unwrap_or_default(), JQ_STRING))
        }
        serde_json::Value::Array(items) => {
            if items.is_empty() {
                out.push_str(&mode.ansi("[]", JQ_STRUCT));
                return;
            }
            out.push_str(&mode.ansi("[", JQ_STRUCT));
            out.push('\n');
            for (i, item) in items.iter().enumerate() {
                out.push_str(&child_indent);
                write_json_colored(item, mode, depth + 1, out);
                if i + 1 < items.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&indent);
            out.push_str(&mode.ansi("]", JQ_STRUCT));
        }
        serde_json::Value::Object(map) => {
            if map.is_empty() {
                out.push_str(&mode.ansi("{}", JQ_STRUCT));
                return;
            }
            out.push_str(&mode.ansi("{", JQ_STRUCT));
            out.push('\n');
            let len = map.len();
            for (i, (k, val)) in map.iter().enumerate() {
                out.push_str(&child_indent);
                out.push_str(&mode.ansi(&serde_json::to_string(k).unwrap_or_default(), JQ_KEY));
                out.push_str(&mode.ansi(":", JQ_STRUCT));
                out.push(' ');
                write_json_colored(val, mode, depth + 1, out);
                if i + 1 < len {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&indent);
            out.push_str(&mode.ansi("}", JQ_STRUCT));
        }
    }
}

fn status_badge(mode: OutputMode, outcome: &str, unsupported: bool) -> String {
    if unsupported {
        return mode.ansi("[UNSUPPORTED]", ANSI_YELLOW);
    }
    match outcome {
        "ok" => mode.ansi("[OK]", ANSI_GREEN),
        other => mode.ansi(&format!("[FAIL: {other}]"), ANSI_RED),
    }
}

/// Renders one `{format, body}` pair (a command's response, or a UPnP
/// description/SOAP response) as human-readable text, appended to `out`.
fn render_body(mode: OutputMode, format: Option<&str>, body: &serde_json::Value, out: &mut String) {
    match format {
        Some("json") => {
            let mut pretty = String::new();
            write_json_colored(body, mode, 0, &mut pretty);
            out.push_str(&mode.code_block("json", &pretty));
        }
        Some("xml") => {
            if let serde_json::Value::String(s) = body {
                let pretty = pretty_print_xml(mode, s);
                out.push_str(&mode.code_block("xml", &pretty));
            }
        }
        Some(f) if f.starts_with("syslog") => match body {
            serde_json::Value::Object(files) => {
                let mut first = true;
                for (name, content) in files {
                    if !first {
                        out.push('\n');
                    }
                    first = false;
                    out.push_str(&mode.bold(&format!("File: {name}")));
                    out.push('\n');
                    if let serde_json::Value::String(s) = content {
                        out.push_str(&mode.code_block("text", s));
                        out.push('\n');
                    }
                }
            }
            serde_json::Value::String(s) => out.push_str(&mode.code_block("text", s)),
            _ => {}
        },
        Some("base64") => {
            if let serde_json::Value::String(s) = body {
                out.push_str(&mode.dim(&format!("<binary data, {} base64 chars, not human-readable>", s.len())));
            }
        }
        // "text", "base64 (decoded for display)", or anything else — plain
        // string bodies print as-is; anything not a string (shouldn't
        // happen outside "json") falls back to the colored JSON printer.
        _ => match body {
            serde_json::Value::String(s) => out.push_str(&mode.code_block("text", s)),
            other => {
                let mut pretty = String::new();
                write_json_colored(other, mode, 0, &mut pretty);
                out.push_str(&pretty);
            }
        },
    }
}

/// Renders one entry of the `commands` array as a delimited block: name,
/// status badge, summary/url/attempts/http_status/error, then the body.
fn render_command(mode: OutputMode, cmd: &serde_json::Value) -> String {
    let get_str = |k: &str| cmd.get(k).and_then(|v| v.as_str());
    let command = get_str("command").unwrap_or("?");
    let outcome = get_str("outcome").unwrap_or("?");
    let unsupported = cmd.get("unsupported").and_then(|v| v.as_bool()).unwrap_or(false);

    let mut out = String::new();
    out.push_str(&mode.rule());
    out.push('\n');
    out.push_str(&mode.heading(3, command));
    out.push(' ');
    out.push_str(&status_badge(mode, outcome, unsupported));
    out.push('\n');

    if let Some(s) = get_str("summary") {
        out.push_str(&mode.dim(s));
        out.push('\n');
    }

    let mut meta = Vec::new();
    if let Some(u) = get_str("url") {
        meta.push(format!("url: {u}"));
    }
    if let Some(a) = cmd.get("attempts").and_then(|v| v.as_u64()) {
        meta.push(format!("attempts: {a}"));
    }
    if let Some(h) = cmd.get("http_status").and_then(|v| v.as_u64()) {
        meta.push(format!("http_status: {h}"));
    }
    if let Some(e) = get_str("error") {
        meta.push(format!("error: {e}"));
    }
    if !meta.is_empty() {
        out.push_str(&mode.dim(&meta.join("  \u{b7}  ")));
        out.push('\n');
    }

    if let Some(body) = cmd.get("body") {
        out.push('\n');
        render_body(mode, get_str("format"), body, &mut out);
        out.push('\n');
    }
    out
}

/// Renders one UPnP SOAP action (`UpnpActionCapture`), same shape as
/// `render_command` but with `service`/`action`/`control_url` instead of
/// `command`/`url`.
fn render_upnp_action(mode: OutputMode, action: &serde_json::Value) -> String {
    let get_str = |k: &str| action.get(k).and_then(|v| v.as_str());
    let name = format!("{}::{}", get_str("service").unwrap_or("?"), get_str("action").unwrap_or("?"));
    let outcome = get_str("outcome").unwrap_or("?");

    let mut out = String::new();
    out.push_str(&mode.rule());
    out.push('\n');
    out.push_str(&mode.heading(3, &name));
    out.push(' ');
    out.push_str(&status_badge(mode, outcome, false));
    out.push('\n');

    let mut meta = Vec::new();
    if let Some(u) = get_str("control_url") {
        meta.push(format!("control_url: {u}"));
    }
    if let Some(h) = action.get("http_status").and_then(|v| v.as_u64()) {
        meta.push(format!("http_status: {h}"));
    }
    if let Some(e) = get_str("error") {
        meta.push(format!("error: {e}"));
    }
    if !meta.is_empty() {
        out.push_str(&mode.dim(&meta.join("  \u{b7}  ")));
        out.push('\n');
    }

    if let Some(response) = action.get("response") {
        out.push('\n');
        render_body(
            mode,
            response.get("format").and_then(|v| v.as_str()),
            response.get("body").unwrap_or(&serde_json::Value::Null),
            &mut out,
        );
        out.push('\n');
    }
    out
}

/// Renders the whole capture file: a metadata header, the skip lists (if
/// non-empty), one block per `commands` entry, then a UPnP section (metadata
/// + description + one block per action) if present.
fn render_capture(mode: OutputMode, value: &serde_json::Value) -> String {
    let get_str = |k: &str| value.get(k).and_then(|v| v.as_str());
    let mut out = String::new();

    let model = get_str("model").unwrap_or("unknown");
    out.push_str(&mode.heading(1, &format!("WiiM Capture: {model}")));
    out.push('\n');

    let mut meta = Vec::new();
    if let Some(v) = get_str("captured_at") {
        meta.push(format!("captured_at: {v}"));
    }
    if let Some(v) = get_str("firmware") {
        meta.push(format!("firmware: {v}"));
    }
    if let Some(v) = get_str("hardware") {
        meta.push(format!("hardware: {v}"));
    }
    if let Some(v) = get_str("project") {
        meta.push(format!("project: {v}"));
    }
    if let Some(v) = get_str("tls_scheme") {
        meta.push(format!("tls: {v}"));
    }
    if let Some(v) = value.get("tls_port").and_then(|v| v.as_u64()) {
        meta.push(format!("port: {v}"));
    }
    if !meta.is_empty() {
        out.push_str(&meta.join("\n"));
        out.push('\n');
    }
    if value.get("gave_up").and_then(|v| v.as_bool()).unwrap_or(false) {
        out.push_str(&mode.ansi("gave_up: true", ANSI_RED));
        out.push('\n');
    }
    out.push('\n');

    for key in ["skipped_unsafe", "skipped_not_destructive"] {
        if let Some(serde_json::Value::Array(items)) = value.get(key) {
            if items.is_empty() {
                continue;
            }
            let names: Vec<&str> = items.iter().filter_map(|v| v.as_str()).collect();
            out.push_str(&mode.bold(&format!("{key} ({})", names.len())));
            out.push('\n');
            out.push_str(&names.join(", "));
            out.push_str("\n\n");
        }
    }

    if let Some(serde_json::Value::Array(commands)) = value.get("commands") {
        for cmd in commands {
            out.push_str(&render_command(mode, cmd));
            out.push('\n');
        }
    }

    if let Some(upnp) = value.get("upnp") {
        let get_str = |k: &str| upnp.get(k).and_then(|v| v.as_str());
        out.push_str(&mode.rule());
        out.push('\n');
        out.push_str(&mode.heading(2, "UPnP"));
        out.push('\n');

        let mut meta = Vec::new();
        if let Some(v) = get_str("location") {
            meta.push(format!("location: {v}"));
        }
        if let Some(v) = get_str("description_url") {
            meta.push(format!("description_url: {v}"));
        }
        if let Some(v) = get_str("friendly_name") {
            meta.push(format!("friendly_name: {v}"));
        }
        if let Some(v) = get_str("model_name") {
            meta.push(format!("model_name: {v}"));
        }
        if let Some(v) = get_str("udn") {
            meta.push(format!("udn: {v}"));
        }
        if let Some(serde_json::Value::Array(types)) = upnp.get("service_types") {
            let types: Vec<&str> = types.iter().filter_map(|v| v.as_str()).collect();
            if !types.is_empty() {
                let mut line = String::from("service_types:");
                for t in &types {
                    line.push_str("\n  ");
                    line.push_str(t);
                }
                meta.push(line);
            }
        }
        if !meta.is_empty() {
            out.push_str(&meta.join("\n"));
            out.push('\n');
        }

        if let Some(description) = upnp.get("description") {
            out.push('\n');
            out.push_str(&mode.bold("description.xml"));
            out.push('\n');
            render_body(
                mode,
                description.get("format").and_then(|v| v.as_str()),
                description.get("body").unwrap_or(&serde_json::Value::Null),
                &mut out,
            );
            out.push('\n');
        }

        if let Some(scpd) = upnp.get("play_queue_scpd") {
            out.push('\n');
            out.push_str(&mode.bold("PlayQueueSCPD.xml"));
            out.push('\n');
            render_body(
                mode,
                scpd.get("format").and_then(|v| v.as_str()),
                scpd.get("body").unwrap_or(&serde_json::Value::Null),
                &mut out,
            );
            out.push('\n');
        }

        if let Some(serde_json::Value::Array(actions)) = upnp.get("actions") {
            for action in actions {
                out.push_str(&render_upnp_action(mode, action));
                out.push('\n');
            }
        }
    }

    out
}

// ── CLI arguments ────────────────────────────────────────────────────────────

struct Args {
    path: String,
    markdown: bool,
}

fn usage() -> ! {
    eprintln!("usage: wiim-capdump [--markdown] <capture-file.json>");
    eprintln!("       wiim-capdump [--markdown] -");
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut path = None;
    let mut markdown = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--markdown" => markdown = true,
            "-h" | "--help" => usage(),
            "-" if path.is_none() => path = Some("-".to_string()),
            other if path.is_none() && !other.starts_with('-') => path = Some(other.to_string()),
            other => {
                eprintln!("wiim-capdump: unrecognized argument '{other}'");
                usage();
            }
        }
    }
    let Some(path) = path else { usage() };
    Args { path, markdown }
}

/// True if `s` looks like an XML document/fragment. Ported from (not shared
/// with) `wiim-capture.rs`'s identical `looks_like_xml` — separate binary
/// crate, can't depend on the other's internals.
fn looks_like_xml(s: &str) -> bool {
    let t = s.trim();
    t.starts_with("<?xml") || (t.starts_with('<') && t.ends_with('>'))
}

/// Renders raw stdin content (from `wiim-capture --one`) as a single blob —
/// format auto-detected the same way `wiim-capture`'s own `encode_blob`
/// decides what to write into a capture file, but without ever falling
/// back to base64: `--one`'s stdout is always meant to be human-readable
/// text already, so anything that isn't JSON or XML just prints as plain
/// text verbatim, whatever it is.
fn render_stdin_blob(mode: OutputMode, raw: &str) -> String {
    let mut out = String::new();
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(v) => render_body(mode, Some("json"), &v, &mut out),
        Err(_) if looks_like_xml(raw) => {
            render_body(mode, Some("xml"), &serde_json::Value::String(raw.to_string()), &mut out)
        }
        Err(_) => render_body(mode, Some("text"), &serde_json::Value::String(raw.to_string()), &mut out),
    }
    out.push('\n');
    out
}

fn main() {
    let args = parse_args();

    let raw = if args.path == "-" {
        let mut buf = String::new();
        if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
            eprintln!("wiim-capdump: failed to read stdin: {e}");
            std::process::exit(1);
        }
        buf
    } else {
        match std::fs::read_to_string(&args.path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("wiim-capdump: failed to read {}: {e}", args.path);
                std::process::exit(1);
            }
        }
    };

    let mode = OutputMode::detect(args.markdown);

    if args.path == "-" {
        print!("{}", render_stdin_blob(mode, &raw));
        return;
    }

    let mut value: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("wiim-capdump: {} is not valid JSON: {e}", args.path);
            std::process::exit(1);
        }
    };

    decode_syslog_entries(&mut value);
    decode_bodies_for_display(&mut value);

    print!("{}", render_capture(mode, &value));
}
