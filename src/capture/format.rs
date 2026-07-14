//! Shared JSON schema for capture files, produced by `wiim-capture` and
//! consumed by `wiim-simulator`. Kept as one definition so the two can't
//! drift apart.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Whether a `commands.yaml` entry reads or mutates device state. Defaults to
/// `Get` when omitted from the YAML — the safe default the whole design
/// leans on (see `safe` below).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    #[default]
    Get,
    Set,
    /// `getsyslog` doesn't return the log itself — it returns a small HTML
    /// page containing a download link that needs a second fetch (see
    /// linkplay-cli's `cli.py::getsyslog`), and that second fetch has been
    /// observed to be slow enough to need a longer timeout than every other
    /// command. `expand_commands()`/`wiim-capture` special-case this method
    /// entirely (see `capture_syslog()`) rather than sending it through the
    /// normal single-GET path; exactly one `commands.yaml` entry should ever
    /// use it. Renders as `"getsyslog"` in YAML via `rename_all = "snake_case"`.
    Getsyslog,
}

/// A `{name}` placeholder in a `CommandSpec::command` template, with the
/// list of concrete values to substitute in (one actual command is sent per
/// value). Independent parameters vary separately; see `CommandSpec::value_sets`
/// for the coordinated-multi-parameter case.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParamSpec {
    pub name: String,
    pub values: Vec<serde_yaml::Value>,
}

/// One entry in `commands.yaml`. `command` may be a bare command
/// (`"getStatusEx"`) or a template with `{name}` placeholders
/// (`"setPlayerCmd:vol:{value}"`), resolved via `params`/`value_sets`.
///
/// `safe` is only ever consulted when `method == Set`; it must default to
/// `false` and is never inferred from the command's name
/// (`getMvRemoteUpdateStart` starts a firmware update despite its
/// `get`-looking name).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandSpec {
    pub command: String,
    #[serde(default)]
    pub method: Method,
    #[serde(default)]
    pub safe: bool,
    #[serde(default)]
    pub params: Vec<ParamSpec>,
    #[serde(default)]
    pub value_sets: Vec<HashMap<String, serde_yaml::Value>>,
    /// When true, percent-encodes everything after the command name's first
    /// `:` (i.e. the fully-substituted argument portion) before it's placed
    /// into the request URL — for commands whose argument can contain
    /// characters (spaces, extra `:`, `/`, etc.) that aren't safe to embed
    /// literally in a query string. Applied after `{name}` substitution, to
    /// the whole remainder (not per-parameter), so a template with several
    /// placeholders and literal separators between them still gets encoded
    /// as one unit.
    #[serde(default)]
    pub urlencode: bool,
    // Optional, purely descriptive — never affects substitution or safety,
    // just echoed into the matching CommandCapture record on output.
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub operation_id: Option<String>,
}

/// How a captured response body is represented in the output JSON.
/// Blob-encoding rule: JSON parses as JSON, else XML-looking content
/// (checked before the plain-text tier, since it may contain `"` from
/// attributes) stays human-readable XML, else plain printable ASCII with no
/// `"` stays human-readable text, else base64. XML only ever comes from the
/// UPnP capture (description.xml, SOAP action responses) in practice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseFormat {
    Json,
    Xml,
    Text,
    Base64,
}

/// How a single HTTP attempt (or an entire command, after retries) turned
/// out. Connection failures are retried (see `wiim-capture`'s retry loop);
/// HTTP errors and protocol errors are not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    HttpError,
    ConnectionError,
    ProtocolError,
}

/// A format+body pair, reused wherever a raw response needs to be captured
/// (command bodies, UPnP description.xml, SOAP action responses).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Blob {
    pub format: ResponseFormat,
    pub body: serde_json::Value,
}

/// The result of sending one fully-substituted command string (e.g.
/// `"setPlayerCmd:vol:0"`, not the `{value}` template) to the device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandCapture {
    pub command: String,
    pub url: String,
    pub attempts: u32,
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<ResponseFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,
    /// True when a 200-OK body is literally "unknown command"/"Failed"/
    /// "unknown" (case-insensitive) — LinkPlay's way of saying "not
    /// supported," not a real payload.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unsupported: bool,
    /// Hex+HTML-entity-decoded companion for player-status Title/Artist/
    /// Album fields, when applicable. Never replaces `body`, which stays
    /// exactly what the device sent (needed for faithful replay).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decoded: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
}

/// One standard read-only UPnP SOAP action attempted against a discovered
/// `AVTransport:1`/`RenderingControl:1` service.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpnpActionCapture {
    pub service: String,
    pub action: String,
    pub control_url: String,
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<Blob>,
}

/// Basic, read-only UPnP capture: SSDP discovery response, device-description
/// XML (fetched via SSDP's LOCATION or, as a fallback, the two well-known
/// LinkPlay UPnP ports directly), and a handful of standard GetXxx SOAP
/// actions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpnpCapture {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssdp_response: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssdp_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<Blob>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub friendly_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udn: Option<String>,
    #[serde(default)]
    pub service_types: Vec<String>,
    #[serde(default)]
    pub has_playqueue: bool,
    #[serde(default)]
    pub has_qplay: bool,
    #[serde(default)]
    pub has_content_directory: bool,
    /// Raw `PlayQueueSCPD.xml` body, when the device advertises the
    /// LinkPlay-proprietary `PlayQueue` service (`has_playqueue`). This is
    /// the service's actual declared action/argument list — read it
    /// directly rather than guessing at what `actions` (below) probes,
    /// since that only ever calls the subset of declared actions that look
    /// read-only (see `wiim-capture`'s `capture_playqueue()`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub play_queue_scpd: Option<Blob>,
    #[serde(default)]
    pub actions: Vec<UpnpActionCapture>,
}

/// Outcome of one tcpuart command send/receive attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TcpUartOutcome {
    Ok,
    NoResponse,
    ConnectionError,
}

/// One GET-only command sent over the raw TCP UART pass-through protocol
/// (port 8899, see `device::tcpuart`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TcpUartCommandCapture {
    /// The ASCII payload sent, e.g. "MCU+VOL+GET" (not the wrapped packet).
    pub command: String,
    pub outcome: TcpUartOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Raw bytes received in the read window after sending this command,
    /// base64-encoded, exactly as received off the socket — the *whole*
    /// packet (header included), not just the decoded payload, so
    /// wiim-capdump can hexdump/validate the framing itself. May be
    /// absent (outcome NoResponse), and may contain more than one packet
    /// if an unsolicited push arrived alongside the reply, or a partial
    /// packet if the read window elapsed mid-message — no framing is
    /// assumed or stripped here, this is exactly what came off the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_base64: Option<String>,
}

/// Raw TCP UART pass-through capture (port 8899) — see `device::tcpuart`.
/// Read-only (GET-only commands) by construction.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TcpUartCapture {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect_error: Option<String>,
    #[serde(default)]
    pub commands: Vec<TcpUartCommandCapture>,
}

/// Top-level capture file written by `wiim-capture`.
///
/// Deliberately has no `target_ip` field — the real IP is scrubbed from
/// every other field (`CommandCapture.url`, UPnP URLs, `<UDN>` etc.), so
/// keeping the plain IP around as a top-level field would undo that on
/// every single capture file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureFile {
    /// RFC 3339, UTC.
    pub captured_at: String,
    pub gave_up: bool,
    /// "unknown" when `gave_up` (or model detection otherwise failed to
    /// produce a usable name).
    pub model: String,
    /// Which of getStatusEx/getStatus produced `model` (and the
    /// firmware/hardware/project fields below), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hardware: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_scheme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_port: Option<u16>,
    pub commands: Vec<CommandCapture>,
    /// Raw `command` templates from `commands.yaml` that were present but
    /// never sent because `method == Set` and `safe != true` — these are
    /// never sent regardless of `--destructive`.
    #[serde(default)]
    pub skipped_unsafe: Vec<String>,
    /// Raw `command` templates that are `method == Set` and `safe == true`
    /// (so *would* run under `--destructive`) but were skipped because
    /// `wiim-capture` was invoked without that flag — `wiim-capture` does
    /// not mutate device state by default.
    #[serde(default)]
    pub skipped_not_destructive: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upnp: Option<UpnpCapture>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcpuart: Option<TcpUartCapture>,
}
