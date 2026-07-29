/// Canonical multiroom group state, decoupled from the wire formats that
/// populate it (LinkPlay `getStatusEx`/`multiroom:getSlaveList` over HTTP,
/// or `SlaveFlag`/`MasterUUID`/`SlaveList` inside a single UPnP
/// `GetInfoEx`).
///
/// This module owns:
/// - `GroupState`/`GroupMember` and their component enums — what the rest
///   of the app consumes. `ui/` never sees a raw `group` string, a
///   `SlaveFlag`, or a slave-list JSON blob.
/// - The decoders that turn either transport's representation into that
///   canonical form, tolerating four generations of `getSlaveList` schema.
/// - UUID normalisation, which is how a follower finds its leader — the
///   only identity key that works across all of them.
///
/// Two wire facts drive most of the shape here, both confirmed against
/// real captures:
///
/// - **A leader's own `group` field reads `"0"`.** `group` answers "am I a
///   follower", not "am I in a group", so a leader is only ever detected
///   via its slave list. Testing `group != "0"` misses every leader.
/// - **A follower does not always know its leader's address.** Some
///   families report `master_uuid` with no `master_ip` at all, and under
///   WiFi-Direct grouping a reported member address can be a `10.10.10.x`
///   one that is not routable from this host. Resolution is therefore
///   always by uuid; addresses are hints.

use std::rc::Rc;

use super::utils;

// ── Canonical types ───────────────────────────────────────────────────────────

/// This device's position in a multiroom group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GroupRole {
    /// Not grouped.
    #[default]
    Standalone,
    /// Has at least one follower. Owns the group's playback.
    Leader,
    /// Renders another device's stream.
    Follower,
}

/// What kind of group this is. Both kinds report the same leader/follower
/// roles, so this is a separate axis rather than a `GroupRole` variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GroupKind {
    /// Every member plays the same stream in sync.
    #[default]
    Multiroom,
    /// A hub decodes multichannel audio and sends one channel per member.
    HomeTheater,
}

/// What a member actually renders.
///
/// `Stereo`/`Left`/`Right` are the long-standing encoding (`0`/`1`/`2`) and
/// are confirmed live. The home-theatre roles are declared but not yet
/// decoded from anything — the field that carries them has only ever been
/// observed unassigned, so they exist to keep the type stable rather than
/// because a decoder produces them today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChannelRole {
    #[default]
    Stereo,
    Left,
    Right,
    Center,
    SurroundLeft,
    SurroundRight,
    Subwoofer,
    /// Preserved verbatim rather than collapsed to `Stereo`. The
    /// home-theatre encoding is not yet known, so an unrecognised value is
    /// far more likely to be a real role we cannot name than a
    /// stereo member — silently mislabelling it would be worse than
    /// admitting ignorance.
    Unknown(i32),
}

impl ChannelRole {
    /// Decodes the classic per-member `channel` value. Values outside the
    /// known set are kept as `Unknown`.
    fn from_channel(raw: i32) -> Self {
        match raw {
            0 => Self::Stereo,
            1 => Self::Left,
            2 => Self::Right,
            other => Self::Unknown(other),
        }
    }
}

/// One follower, as reported by its leader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupMember {
    /// Normalised uuid
    pub uuid:   String,
    pub name:   String,
    /// As reported by the leader. May be a `10.10.10.x` address that is
    /// unreachable from this host when the leader groups over WiFi-Direct,
    /// so never use this to identify a member — only to address one, and
    /// only after checking it is routable.
    pub ip:     String,
    pub volume: u8,
    pub muted:  bool,
    pub role:   ChannelRole,
    /// A masked member stays in the group but behaves as a standalone
    /// device.
    pub masked: bool,
}

/// One device's view of its group. Lives once per device and is updated in
/// place rather than rebuilt.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GroupState {
    pub role: GroupRole,
    pub kind: GroupKind,
    /// Normalised. Set for a `Follower` (its leader) and for a `Leader`
    /// (itself, when the device reports its own uuid); `None` when
    /// unknown.
    pub leader_uuid: Option<String>,
    /// `None` on families that report only `master_uuid`, which is why
    /// `leader_uuid` is the field resolution actually depends on.
    pub leader_ip: Option<String>,
    /// Followers, leader-side only. Always empty on a `Follower` — a
    /// follower's own slave list is empty on every device observed.
    /// `Rc` so cloning a `GroupState` out of a `RefCell` stays a refcount
    /// bump, matching how playback state is handled.
    pub members: Rc<Vec<GroupMember>>,
}

impl GroupState {
    pub fn is_grouped(&self) -> bool {
        self.role != GroupRole::Standalone
    }

    /// Whether two snapshots describe the same group *shape* — same role,
    /// same kind, same leader, same members in the same order and channel
    /// assignment.
    ///
    /// Deliberately ignores each member's `volume` and `muted`. Those change
    /// constantly and are not structural: a listener that rebuilds a device
    /// list on every topology change must not be woken by somebody nudging
    /// a slider, or it tears down the very widget being dragged. Callers
    /// that care about levels compare those separately.
    pub fn same_topology(&self, other: &Self) -> bool {
        if self.role != other.role
            || self.kind != other.kind
            || self.leader_uuid != other.leader_uuid
            || self.leader_ip != other.leader_ip
            || self.members.len() != other.members.len()
        {
            return false;
        }
        self.members.iter().zip(other.members.iter()).all(|(a, b)| {
            a.uuid == b.uuid
                && a.name == b.name
                && a.ip == b.ip
                && a.role == b.role
                && a.masked == b.masked
        })
    }

    /// Copies each member's `volume`/`muted` across from `previous`, matched
    /// by uuid, leaving everything else in `self` alone.
    ///
    /// Used to protect optimistic levels from a poll that has not caught up
    /// yet. A member's own `DeviceState` has its own settle window for
    /// exactly this, but a member with no live `DeviceState` is only
    /// represented by these cached values, and without this a slave list
    /// reporting pre-command levels would undo the group adjustment that
    /// was just made.
    ///
    /// A member absent from `previous` keeps whatever it arrived with —
    /// there is nothing optimistic to preserve for a member that has only
    /// just appeared.
    pub fn carry_levels_from(&mut self, previous: &Self) {
        if previous.members.is_empty() {
            return;
        }
        let members = Rc::make_mut(&mut self.members);
        for m in members.iter_mut() {
            if let Some(old) = previous.members.iter().find(|p| p.uuid == m.uuid) {
                m.volume = old.volume;
                m.muted  = old.muted;
            }
        }
    }

    /// Follower count. Zero unless this device is a leader.
    pub fn follower_count(&self) -> usize {
        self.members.len()
    }
}

/// The group's volume for a set of member levels: the loudest of them.
///
/// `levels` must include every member the group's volume should reflect,
/// the leader among them. Zero for an empty group, which no caller should
/// be asking about.
pub fn group_volume_of(levels: &[u32]) -> u32 {
    levels.iter().copied().max().unwrap_or(0)
}

/// New level for each member when the group volume is moved to `target`.
///
/// The group's volume is the loudest member's, so the requested change is
/// the difference between that and `target`, and every member shifts by
/// that same amount. Levels are *shifted*, never equalised: members at 10
/// and 20 asked for 15 end up at 5 and 15.
///
/// **Clamping is hard and is not restored.** A member that reaches a rail
/// stops there, and a later move in the other direction starts from the
/// rail rather than from where the member would notionally have been.
/// Continuing that example, a further -10 gives 0 and 5 (the first would
/// have gone to -5) and a subsequent +10 gives 10 and 15 — five apart now
/// rather than the original ten. This is what the vendor app does, and it
/// is the easier of the two to reason about: levels visibly compress
/// against the end of their range instead of silently re-expanding from
/// state the user cannot see.
///
/// Returns one level per entry in `levels`, in the same order.
pub fn shift_to_group_volume(levels: &[u32], target: u32) -> Vec<u32> {
    let delta = target as i32 - group_volume_of(levels) as i32;
    levels.iter()
        .map(|v| (*v as i32 + delta).clamp(0, 100) as u32)
        .collect()
}

/// Whether a group counts as muted, given each member's mute state.
///
/// A group is muted only when **every** member is — the logical AND. Any
/// member still making sound means the group is not silent, which is what
/// the control should reflect. Unmuting one member therefore unmutes the
/// group's indicator, while muting one member on its own does not set it.
///
/// `false` for an empty group: nothing is playing, but nothing is muted
/// either, and reporting "muted" for a group with no members would be an
/// odd thing to show.
pub fn group_muted_of(states: &[bool]) -> bool {
    !states.is_empty() && states.iter().all(|m| *m)
}

/// The name a device gives an ad-hoc group of its own: the leader's name
/// followed by its follower count.
///
/// This mirrors the devices' own auto-naming convention exactly (observed
/// live as `"WiiM WorkBu + 0"` becoming `"WiiM WorkBu + 1"` when a follower
/// joined), so a group reads the same here as it does in the vendor app.
/// The count is **followers**, not total members — a two-device group is
/// `+ 1`.
///
/// Named groups are a separate concept the device stores independently;
/// once those are read, a group with a real name should use it in
/// preference to this.
pub fn auto_group_name(leader_name: &str, follower_count: usize) -> String {
    format!("{leader_name} + {follower_count}")
}

// ── Lenient JSON access ───────────────────────────────────────────────────────
//
// Four schema generations are in the wild and they disagree on both key
// casing and scalar type: the oldest documented shape uses capitalised keys
// (`Slaves`, `Slave_list`, `Name`, `Ip`) with every value quoted as a
// string, while current firmware uses lowercase keys and real JSON numbers.
// Accessors here absorb both so the decoder below reads as one code path.

/// Case-insensitive object lookup.
fn field<'a>(obj: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    let map = obj.as_object()?;
    // Exact hit first: the overwhelmingly common case, and avoids walking
    // the map for every field of every member.
    if let Some(v) = map.get(key) {
        return Some(v);
    }
    map.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v)
}

/// Reads an integer that may be encoded as a JSON number or a quoted
/// string.
fn int_field(obj: &serde_json::Value, key: &str) -> Option<i64> {
    match field(obj, key)? {
        serde_json::Value::Number(n) => n.as_i64(),
        serde_json::Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn str_field(obj: &serde_json::Value, key: &str) -> Option<String> {
    match field(obj, key)? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Truthiness for a flag that may be `0`/`1`, `"0"`/`"1"`, or a real bool.
fn bool_field(obj: &serde_json::Value, key: &str) -> bool {
    match field(obj, key) {
        Some(serde_json::Value::Bool(b)) => *b,
        _ => int_field(obj, key).is_some_and(|v| v != 0),
    }
}

// ── Slave-list decoding ───────────────────────────────────────────────────────

/// What a `multiroom:getSlaveList` response (or the identical JSON embedded
/// in a UPnP `GetInfoEx` `<SlaveList>`) says about this device's followers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SlaveList {
    pub members: Vec<GroupMember>,
    pub kind:    GroupKind,
}

/// Decodes a slave-list payload.
///
/// Returns `None` only when the payload is not a JSON object at all — which
/// is how the families that do not implement the call answer it (a bare
/// `OK`), and must not be mistaken for "no followers". An object with no
/// list is a real, empty answer.
pub fn decode_slave_list(raw: &str) -> Option<SlaveList> {
    let root: serde_json::Value = serde_json::from_str(raw.trim()).ok()?;
    if !root.is_object() {
        return None;
    }

    // `surround` only exists on the newest schema; its absence means an
    // ordinary multiroom group, not an unknown one.
    let kind = if int_field(&root, "surround").is_some_and(|v| v != 0) {
        GroupKind::HomeTheater
    } else {
        GroupKind::Multiroom
    };

    let entries = field(&root, "slave_list")
        .and_then(|v| v.as_array())
        // The oldest schema spells it `Slave_list`; `field` already handles
        // that. This fallback covers firmware that puts the array directly
        // in `slaves` instead of a count.
        .or_else(|| field(&root, "slaves").and_then(|v| v.as_array()));

    let members = entries
        .map(|arr| arr.iter().filter_map(decode_member).collect())
        .unwrap_or_default();

    Some(SlaveList { members, kind })
}

fn decode_member(entry: &serde_json::Value) -> Option<GroupMember> {
    let uuid = str_field(entry, "uuid").map(|u| utils::normalize_uuid(&u)).unwrap_or_default();
    let ip = str_field(entry, "ip").unwrap_or_default();
    // An entry with neither identity nor address is unusable; everything
    // else is best-effort.
    if uuid.is_empty() && ip.is_empty() {
        return None;
    }
    Some(GroupMember {
        uuid,
        name: str_field(entry, "name").unwrap_or_default(),
        ip,
        volume: int_field(entry, "volume").unwrap_or(0).clamp(0, 100) as u8,
        muted: bool_field(entry, "mute"),
        role: ChannelRole::from_channel(int_field(entry, "channel").unwrap_or(0) as i32),
        masked: bool_field(entry, "mask"),
    })
}

// ── Role detection ────────────────────────────────────────────────────────────

/// The per-device fields either transport supplies, gathered into one shape
/// so role detection has a single implementation rather than one per
/// backend.
#[derive(Debug, Clone, Default)]
pub struct GroupInputs {
    /// This device's own uuid, used to reject a device that names itself as
    /// its own leader.
    pub self_uuid: String,
    /// HTTP `getStatusEx`'s `group`. `"0"`/absent means "not a follower".
    pub group_field: Option<String>,
    /// UPnP `GetInfoEx`'s `<SlaveFlag>`, when that transport was used.
    pub slave_flag: Option<bool>,
    pub master_uuid: Option<String>,
    pub master_ip: Option<String>,
    /// Decoded slave list, when one was obtained. `None` means "not asked
    /// or not supported", which is distinct from an empty list.
    pub slave_list: Option<SlaveList>,
}

/// Resolves a device's group role from whichever inputs are available.
///
/// Follower is tested before leader deliberately. The two are not mutually
/// exclusive on the wire — a device that has just left a group can briefly
/// report a stale slave list — and a device that knows it has a leader is
/// the more trustworthy of the two signals.
pub fn detect(inputs: &GroupInputs) -> GroupState {
    let master_uuid = inputs
        .master_uuid
        .as_deref()
        .map(utils::normalize_uuid)
        .filter(|u| !u.is_empty());
    let master_ip = inputs.master_ip.as_deref().map(str::trim).filter(|s| !s.is_empty());

    // A device naming itself as its own master is reporting "no leader" in
    // an awkward way; treat it as unset rather than as a self-referential
    // group.
    let names_self = master_uuid
        .as_deref()
        .is_some_and(|m| utils::same_device(m, &inputs.self_uuid));

    let group_says_follower = inputs
        .group_field
        .as_deref()
        .map(str::trim)
        .is_some_and(|g| !g.is_empty() && g != "0");

    let flagged_follower = inputs.slave_flag.unwrap_or(false);

    if (flagged_follower || group_says_follower) && !names_self && (master_uuid.is_some() || master_ip.is_some()) {
        return GroupState {
            role: GroupRole::Follower,
            // A follower is told nothing about the group's shape, so the
            // kind is not knowable from this side.
            kind: GroupKind::Multiroom,
            leader_uuid: master_uuid,
            leader_ip: master_ip.map(str::to_owned),
            members: Rc::new(Vec::new()),
        };
    }

    if let Some(list) = &inputs.slave_list {
        if !list.members.is_empty() {
            let self_uuid = utils::normalize_uuid(&inputs.self_uuid);
            return GroupState {
                role: GroupRole::Leader,
                kind: list.kind,
                leader_uuid: (!self_uuid.is_empty()).then_some(self_uuid),
                leader_ip: None,
                members: Rc::new(list.members.clone()),
            };
        }
    }

    GroupState::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::format::CaptureFile;

    // Fixtures are real capture files rather than transcribed wire strings,
    // so a schema detail nobody thought to hand-copy still gets exercised.
    // The four loaded here deliberately span every slave-list generation:
    // a 4.3 leader and follower, a 4.2 follower that reports no master
    // address, and a pre-4.2 device whose entire response is `{"slaves":0}`.

    fn load(filename: &str) -> CaptureFile {
        let path = format!("{}/captures/test-devices/{filename}", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading fixture {path}: {e}"));
        serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("parsing fixture {path}: {e}"))
    }

    /// A capture records every probe attempt, so the same command can appear
    /// several times with the earlier entries being connection failures
    /// against TLS modes the device does not speak. Take the first one that
    /// actually returned something.
    fn command_body(cap: &CaptureFile, command: &str) -> serde_json::Value {
        cap.commands
            .iter()
            .filter(|c| c.command == command)
            .find_map(|c| c.body.clone())
            .unwrap_or_else(|| panic!("capture has no successful {command}"))
    }

    /// Capture bodies are stored parsed for JSON responses, so re-serialise
    /// to feed the decoder the string form it takes from the wire.
    fn command_text(cap: &CaptureFile, command: &str) -> String {
        match command_body(cap, command) {
            serde_json::Value::String(s) => s,
            other => other.to_string(),
        }
    }

    /// Builds detection inputs from a capture's `getStatusEx`.
    ///
    /// `self_uuid` is supplied rather than read from the capture because the
    /// capture anonymiser rewrites **every** uuid to the same placeholder —
    /// a follower fixture has `uuid` and `master_uuid` both reading
    /// `xxxx…`, which is indistinguishable from a device naming itself as
    /// its own leader. Real hardware never does that; the fixture only looks
    /// that way because the identifying values were scrubbed. Passing a
    /// distinct value restores the distinction the wire actually had.
    fn inputs_from_status(cap: &CaptureFile, self_uuid: &str, slave_list: Option<SlaveList>) -> GroupInputs {
        let status = command_body(cap, "getStatusEx");
        GroupInputs {
            self_uuid: self_uuid.to_owned(),
            group_field: str_field(&status, "group"),
            slave_flag: None,
            master_uuid: str_field(&status, "master_uuid"),
            master_ip: str_field(&status, "master_ip"),
            slave_list,
        }
    }

    /// Stand-in for the device's own uuid in fixtures whose real one was
    /// anonymised away. Deliberately unlike the anonymiser's placeholder.
    const SELF_UUID: &str = "0123456789abcdef01234567";

    const LEADER: &str = "WiiM_Ultra_20260708_022203.group1.json";
    const FOLLOWER: &str = "WiiM_Ultra_20260708_022221.group2.json";
    const FOLLOWER_NO_MASTER_IP: &str = "AudioCast_iEAST-02_20260714_030113.json";
    const OLDEST: &str = "Audio_Pro_Addon_C5_20260710_073433.FW3.7.x.json";
    const STANDALONE: &str = "WiiM_Sound_20260726_180153.json";

    #[test]
    fn decodes_a_real_leaders_slave_list() {
        let cap = load(LEADER);
        let list = decode_slave_list(&command_text(&cap, "multiroom:getSlaveList"))
            .expect("leader slave list should decode");

        assert_eq!(list.kind, GroupKind::Multiroom);
        assert_eq!(list.members.len(), 1);
        let m = &list.members[0];
        assert_eq!(m.name, "WiiM Bu");
        assert_eq!(m.volume, 30);
        assert!(!m.muted);
        assert!(!m.masked);
        assert_eq!(m.role, ChannelRole::Stereo);
        assert!(!m.uuid.is_empty());
        // Anonymised captures keep the shape but scrub the value; the point
        // is that a member is identifiable at all.
        assert_eq!(m.uuid, utils::normalize_uuid(&m.uuid));
    }

    #[test]
    fn leader_is_detected_despite_its_group_field_reading_zero() {
        let cap = load(LEADER);
        // The trap this whole module is shaped around.
        let status = command_body(&cap, "getStatusEx");
        assert_eq!(str_field(&status, "group").as_deref(), Some("0"));

        let list = decode_slave_list(&command_text(&cap, "multiroom:getSlaveList"));
        let state = detect(&inputs_from_status(&cap, SELF_UUID, list));

        assert_eq!(state.role, GroupRole::Leader);
        assert_eq!(state.follower_count(), 1);
        assert!(state.is_grouped());
    }

    #[test]
    fn follower_is_detected_and_reports_its_leader() {
        let cap = load(FOLLOWER);
        let list = decode_slave_list(&command_text(&cap, "multiroom:getSlaveList"));
        let state = detect(&inputs_from_status(&cap, SELF_UUID, list));

        assert_eq!(state.role, GroupRole::Follower);
        assert_eq!(state.follower_count(), 0);
        assert!(state.leader_uuid.is_some());
        assert!(state.leader_ip.is_some());
    }

    #[test]
    fn follower_without_master_ip_still_resolves_by_uuid() {
        let cap = load(FOLLOWER_NO_MASTER_IP);
        let status = command_body(&cap, "getStatusEx");
        // This family genuinely omits the address, which is why uuid is the
        // identity key everywhere else in this module.
        assert!(str_field(&status, "master_ip").is_none());

        let list = decode_slave_list(&command_text(&cap, "multiroom:getSlaveList"));
        let state = detect(&inputs_from_status(&cap, SELF_UUID, list));

        assert_eq!(state.role, GroupRole::Follower);
        assert!(state.leader_uuid.is_some());
        assert_eq!(state.leader_ip, None);
    }

    #[test]
    fn standalone_devices_decode_as_standalone() {
        for fixture in [STANDALONE, OLDEST] {
            let cap = load(fixture);
            let list = decode_slave_list(&command_text(&cap, "multiroom:getSlaveList"));
            // Even the oldest firmware's minimal `{"slaves":0}` is a real
            // answer, not an unsupported one.
            assert!(list.is_some(), "{fixture} slave list should decode");
            assert!(list.as_ref().unwrap().members.is_empty(), "{fixture} has no members");

            let state = detect(&inputs_from_status(&cap, SELF_UUID, list));
            assert_eq!(state.role, GroupRole::Standalone, "{fixture}");
            assert!(!state.is_grouped(), "{fixture}");
        }
    }

    #[test]
    fn unsupported_slave_list_is_distinct_from_empty() {
        // The families that do not implement the call answer with a bare
        // `OK`. Treating that as "no followers" would be wrong for a device
        // that is in fact leading a group.
        assert!(decode_slave_list("OK").is_none());
        assert!(decode_slave_list("unknown command").is_none());
        assert!(decode_slave_list("").is_none());
        assert!(decode_slave_list("{}").is_some());
    }

    #[test]
    fn decodes_the_oldest_documented_schema() {
        // Capitalised keys and quoted scalars throughout. No device we hold
        // a capture of produces this, so it is covered here rather than by
        // a fixture.
        let raw = r#"{"Slaves":"1","Slave_list":[{"Name":"FA5100_a3f4","Mask":"0",
                      "Volume":"90","Mute":"0","Channel":"1","Ip":"10.10.10.100",
                      "Version":"WIFIAudio.1.2.2321","Uuid":"uuid:AB-CD-ef"}]}"#;
        let list = decode_slave_list(raw).expect("legacy schema should decode");

        assert_eq!(list.members.len(), 1);
        let m = &list.members[0];
        assert_eq!(m.name, "FA5100_a3f4");
        assert_eq!(m.volume, 90);
        assert!(!m.muted);
        assert_eq!(m.role, ChannelRole::Left);
        assert_eq!(m.uuid, "abcdef");
    }

    #[test]
    fn surround_flag_selects_home_theater() {
        assert_eq!(
            decode_slave_list(r#"{"slaves":0,"surround":0,"group_type":-1}"#).unwrap().kind,
            GroupKind::Multiroom,
        );
        assert_eq!(
            decode_slave_list(r#"{"slaves":0,"surround":1,"group_type":-1}"#).unwrap().kind,
            GroupKind::HomeTheater,
        );
    }

    #[test]
    fn unknown_channel_values_are_preserved() {
        let list = decode_slave_list(r#"{"slave_list":[{"ip":"1.2.3.4","channel":7}]}"#).unwrap();
        assert_eq!(list.members[0].role, ChannelRole::Unknown(7));
    }

    #[test]
    fn group_is_muted_only_when_every_member_is() {
        assert!(group_muted_of(&[true, true, true]));
        // One member still audible means the group is not silent.
        assert!(!group_muted_of(&[true, false, true]));
        assert!(!group_muted_of(&[false, false]));
        assert!(group_muted_of(&[true]));
        // Nothing to mute is not the same as muted.
        assert!(!group_muted_of(&[]));
    }

    #[test]
    fn group_volume_is_the_loudest_member() {
        assert_eq!(group_volume_of(&[10, 20]), 20);
        assert_eq!(group_volume_of(&[20, 10]), 20);
        assert_eq!(group_volume_of(&[7]), 7);
        // Raising a quiet member past the previous loudest makes it the
        // group volume, with no special handling anywhere.
        assert_eq!(group_volume_of(&[10, 95]), 95);
        assert_eq!(group_volume_of(&[]), 0);
    }

    #[test]
    fn moving_the_group_shifts_every_member_by_the_same_amount() {
        // The worked example this behaviour was specified from.
        let start = [10u32, 20];
        assert_eq!(group_volume_of(&start), 20);

        // Reduce by 5: 20 -> 15.
        let a = shift_to_group_volume(&start, 15);
        assert_eq!(a, vec![5, 15]);

        // Reduce by a further 10: 15 -> 5. The first member would have gone
        // to -5 and clamps at 0 instead.
        let b = shift_to_group_volume(&a, 5);
        assert_eq!(b, vec![0, 5]);

        // Increase by 10: 5 -> 15. The clamped member resumes from 0, not
        // from the -5 it never actually reached, so the two are now 5 apart
        // rather than the original 10.
        let c = shift_to_group_volume(&b, 15);
        assert_eq!(c, vec![10, 15]);
    }

    #[test]
    fn group_volume_shift_clamps_at_both_rails() {
        assert_eq!(shift_to_group_volume(&[95, 100], 100), vec![95, 100]);
        // Asking beyond the top: everything pins, nothing wraps.
        assert_eq!(shift_to_group_volume(&[95, 100], 120), vec![100, 100]);
        // Group volume is 8, so this is a delta of -8: the quieter member
        // would reach -5 and floors instead, and both end up silent.
        assert_eq!(shift_to_group_volume(&[3, 8], 0), vec![0, 0]);
        // A smaller reduction keeps the quieter member above the floor.
        assert_eq!(shift_to_group_volume(&[3, 8], 6), vec![1, 6]);
        // A no-op request leaves every member exactly as it was.
        assert_eq!(shift_to_group_volume(&[10, 20], 20), vec![10, 20]);
    }

    #[test]
    fn carrying_levels_preserves_optimistic_values_without_touching_topology() {
        let optimistic = GroupState {
            role: GroupRole::Leader,
            members: Rc::new(vec![
                GroupMember { uuid: "aa".into(), name: "A".into(), ip: "1.1.1.1".into(),
                              volume: 13, muted: true, role: ChannelRole::Stereo, masked: false },
                GroupMember { uuid: "bb".into(), name: "B".into(), ip: "1.1.1.2".into(),
                              volume: 13, muted: true, role: ChannelRole::Stereo, masked: false },
            ]),
            ..Default::default()
        };
        // What a poll that has not caught up reports: stale levels, and a
        // rename that *is* real and must survive.
        let mut polled = GroupState {
            role: GroupRole::Leader,
            members: Rc::new(vec![
                GroupMember { uuid: "aa".into(), name: "A renamed".into(), ip: "1.1.1.1".into(),
                              volume: 2, muted: false, role: ChannelRole::Left, masked: false },
                GroupMember { uuid: "cc".into(), name: "C".into(), ip: "1.1.1.3".into(),
                              volume: 40, muted: false, role: ChannelRole::Stereo, masked: false },
            ]),
            ..Default::default()
        };
        polled.carry_levels_from(&optimistic);

        // Levels come from the optimistic side...
        assert_eq!(polled.members[0].volume, 13);
        assert!(polled.members[0].muted);
        // ...but nothing else does.
        assert_eq!(polled.members[0].name, "A renamed");
        assert_eq!(polled.members[0].role, ChannelRole::Left);
        // A member that has only just appeared has nothing to preserve.
        assert_eq!(polled.members[1].uuid, "cc");
        assert_eq!(polled.members[1].volume, 40);
    }

    #[test]
    fn topology_comparison_ignores_member_levels() {
        let base = GroupState {
            role: GroupRole::Leader,
            members: Rc::new(vec![GroupMember {
                uuid: "aabb".into(), name: "Kitchen".into(), ip: "10.0.0.2".into(),
                volume: 30, muted: false, role: ChannelRole::Stereo, masked: false,
            }]),
            ..Default::default()
        };

        // A slider nudge must not read as a topology change, or the device
        // list rebuilds mid-drag and destroys the slider.
        let mut louder = base.clone();
        Rc::make_mut(&mut louder.members)[0].volume = 55;
        assert!(base.same_topology(&louder));
        assert_ne!(base, louder, "the levels really did differ");

        let mut muted = base.clone();
        Rc::make_mut(&mut muted.members)[0].muted = true;
        assert!(base.same_topology(&muted));

        // Anything structural must still register.
        let mut renamed = base.clone();
        Rc::make_mut(&mut renamed.members)[0].name = "Lounge".into();
        assert!(!base.same_topology(&renamed));

        let mut rechannelled = base.clone();
        Rc::make_mut(&mut rechannelled.members)[0].role = ChannelRole::Left;
        assert!(!base.same_topology(&rechannelled));

        let mut gone = base.clone();
        Rc::make_mut(&mut gone.members).clear();
        assert!(!base.same_topology(&gone));

        let mut demoted = base.clone();
        demoted.role = GroupRole::Standalone;
        assert!(!base.same_topology(&demoted));
    }

    #[test]
    fn wifi_direct_addresses_are_recognised_as_unroutable() {
        assert!(utils::is_wifi_direct_address("10.10.10.92"));
        assert!(utils::is_wifi_direct_address(" 10.10.10.254 "));
        // An ordinary LAN address that merely starts with 10.
        assert!(!utils::is_wifi_direct_address("10.1.1.77"));
        assert!(!utils::is_wifi_direct_address("192.168.1.5"));
        assert!(!utils::is_wifi_direct_address(""));
    }

    #[test]
    fn uuid_normalisation_spans_the_shapes_seen_on_the_wire() {
        let canonical = "ff98f7f4075b5a90fa9572c3";
        for raw in [
            "FF98F7F4075B5A90FA9572C3",
            "uuid:FF98F7F4075B5A90FA9572C3",
            "uuid:ff98f7f4-075b-5a90-fa95-72c3",
            "  FF98F7F4075B5A90FA9572C3  ",
            // The prefix is not always spelled in lower case, and a missed
            // one survives the alphanumeric filter as four stray letters
            // rather than disappearing.
            "UUID:FF98F7F4-075B-5A90-FA95-72C3",
            "Uuid:ff98f7f4075b5a90fa9572c3",
        ] {
            assert_eq!(utils::normalize_uuid(raw), canonical, "{raw}");
        }
        assert!(utils::same_device("uuid:AB-CD", "abcd"));
        // Two unknowns must never compare equal.
        assert!(!utils::same_device("", ""));
    }

    #[test]
    fn a_device_naming_itself_as_master_is_not_a_follower() {
        // Observed as a real recovery hazard after a botched ungroup: the
        // device still reports group="1" but points the master fields at
        // itself.
        let state = detect(&GroupInputs {
            self_uuid: "AABBCC".into(),
            group_field: Some("1".into()),
            master_uuid: Some("uuid:aabbcc".into()),
            ..Default::default()
        });
        assert_eq!(state.role, GroupRole::Standalone);
    }

    #[test]
    fn follower_flag_wins_over_a_stale_slave_list() {
        // A device that has just been demoted can report both. Trusting the
        // slave list here would render it as a leader of devices it no
        // longer controls.
        let state = detect(&GroupInputs {
            self_uuid: "SELF".into(),
            slave_flag: Some(true),
            master_uuid: Some("LEADER".into()),
            slave_list: decode_slave_list(r#"{"slave_list":[{"ip":"1.2.3.4","uuid":"OLD"}]}"#),
            ..Default::default()
        });
        assert_eq!(state.role, GroupRole::Follower);
        assert_eq!(state.follower_count(), 0);
    }

    #[test]
    fn follower_claim_without_any_leader_identity_is_not_trusted() {
        // `group != "0"` alone cannot resolve to a follower: there would be
        // nothing to attach it to in the device list.
        let state = detect(&GroupInputs {
            self_uuid: "SELF".into(),
            group_field: Some("1".into()),
            ..Default::default()
        });
        assert_eq!(state.role, GroupRole::Standalone);
    }
}
