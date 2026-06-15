//! Typed wiring for the server→client NetConnection `onStatus` command
//! and its Enhanced-RTMP-v2 *Reconnect Request* case (Veovera
//! `enhanced-rtmp-v2` §"Reconnect Request", §"Detailed Overview of the
//! onStatus Command for NetConnection").
//!
//! Legacy [RTMP] left `onStatus` largely undocumented; E-RTMP formalises
//! it and extends it with a new `NetConnection.Connect.ReconnectRequest`
//! status event a server uses to ask a client to move to another server
//! instance (live-server updates, geolocation / load-balancing remaps).
//!
//! Unlike `onMetaData` / `onCuePoint` / `onXMPData` (which are FLV
//! `SCRIPTDATA` *tags*), `onStatus` is an RTMP **command message**: an
//! AMF-encoded sequence relayed over the command stream, not framed as
//! an FLV tag. The spec describes the wire shape as four AMF values:
//!
//! | Field          | Type   | Value                                  |
//! | -------------- | ------ | -------------------------------------- |
//! | Command Name   | string | `"onStatus"`                           |
//! | Transaction ID | number | `0` (no response needed)               |
//! | Command Object | null   | always `null` for `onStatus`           |
//! | Info Object    | Object | the status name/value pairs (below)    |
//!
//! The Info Object is the payload. The base `onStatus` Info Object
//! carries `code` / `level` (and optional `description`); the reconnect
//! variant additionally carries an optional `tcUrl`. For reconnect the
//! spec pins two values: `code` MUST be
//! `NetConnection.Connect.ReconnectRequest` and `level` MUST be
//! `status`.
//!
//! This module provides a typed [`OnStatusInfo`] builder, an AMF0
//! command serialiser ([`write_on_status_command`] /
//! [`write_on_status_command_body`]), a [`reconnect_request`]
//! convenience constructor, and a [`parse_on_status_command`] reader
//! that recovers the typed Info Object from the AMF0 command sequence —
//! closing the read↔write loop the same way `colorInfo` does.

use std::io::Write;

use oxideav_core::{Error, Result};

use crate::amf0::{self, parse_amf0_value, AmfValue};

/// AMF0 command name for the NetConnection status command.
pub const COMMAND_NAME: &str = "onStatus";

/// `code` value that signals the Enhanced-RTMP-v2 reconnect request.
/// Per spec: "To reconnect `code` MUST be set to
/// `NetConnection.Connect.ReconnectRequest`."
pub const CODE_RECONNECT_REQUEST: &str = "NetConnection.Connect.ReconnectRequest";

/// `level` value mandated for the reconnect status event. Per spec: "To
/// reconnect the `level` MUST be set to `status`." `status` is also one
/// of the three established `level` values (`status` / `warning` /
/// `error`) for the general `onStatus` Info Object.
pub const LEVEL_STATUS: &str = "status";

/// Typed view of the `onStatus` Info Object — the AMF-encoded
/// name/value pairs that describe a NetConnection status change.
///
/// `code` and `level` are the two properties the spec lists as always
/// present on the general `onStatus` Info Object; `description` is
/// optional ("Not every information object includes this property"), and
/// `tcUrl` is the reconnect-specific optional property (the absolute or
/// relative URI of the server to reconnect to). The Info Object "MAY
/// contain other properties as appropriate to the client"; producers
/// that need extra pairs append them via [`OnStatusInfo::property`] and
/// they are serialised after the four spec-named ones in insertion
/// order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OnStatusInfo {
    /// `code` — a string identifying the event that occurred
    /// (e.g. `NetConnection.Connect.Success`,
    /// `NetConnection.Connect.ReconnectRequest`).
    pub code: String,
    /// `level` — severity of the event: `status`, `warning`, or `error`.
    pub level: String,
    /// `description` (optional) — human-readable detail about the event.
    pub description: Option<String>,
    /// `tcUrl` (optional, reconnect only) — absolute or relative URI of
    /// the server to reconnect to. A relative reference is resolved
    /// against the current connection's `tcUrl`; absent, the client
    /// reuses the current `tcUrl`.
    pub tc_url: Option<String>,
    /// Additional client-appropriate properties, emitted after the
    /// spec-named ones in insertion order. String-valued only — the
    /// reconnect / status Info Object properties are all strings.
    pub extra: Vec<(String, String)>,
}

impl OnStatusInfo {
    /// Build an Info Object with the two always-present properties.
    pub fn new(code: &str, level: &str) -> Self {
        Self {
            code: code.to_string(),
            level: level.to_string(),
            ..Self::default()
        }
    }

    /// Set the optional `description` property.
    #[must_use]
    pub fn description(mut self, description: &str) -> Self {
        self.description = Some(description.to_string());
        self
    }

    /// Set the optional `tcUrl` property (reconnect target server URI).
    #[must_use]
    pub fn tc_url(mut self, tc_url: &str) -> Self {
        self.tc_url = Some(tc_url.to_string());
        self
    }

    /// Append a client-specific extra string property.
    #[must_use]
    pub fn property(mut self, name: &str, value: &str) -> Self {
        self.extra.push((name.to_string(), value.to_string()));
        self
    }

    /// `true` when this Info Object carries the reconnect-request `code`.
    pub fn is_reconnect_request(&self) -> bool {
        self.code == CODE_RECONNECT_REQUEST
    }
}

/// Build the Enhanced-RTMP-v2 reconnect-request Info Object.
///
/// Fills `code = NetConnection.Connect.ReconnectRequest` and
/// `level = status` (both spec-mandated for reconnect). `tc_url` is the
/// optional reconnect target: pass `Some(uri)` to remap the client to a
/// different server instance, or `None` to ask the client to reconnect
/// to the current `tcUrl`. `description` is the optional human-readable
/// reason.
pub fn reconnect_request(tc_url: Option<&str>, description: Option<&str>) -> OnStatusInfo {
    let mut info = OnStatusInfo::new(CODE_RECONNECT_REQUEST, LEVEL_STATUS);
    info.tc_url = tc_url.map(str::to_string);
    info.description = description.map(str::to_string);
    info
}

/// Serialise just the `onStatus` Info Object (the anonymous AMF0 Object
/// value) into `out`. Properties are emitted in the spec table order —
/// `tcUrl` (when present), `code`, `description` (when present),
/// `level` — followed by any [`OnStatusInfo::extra`] pairs. `code` and
/// `level` are always written even if empty, since the spec lists them
/// as the two always-present Info Object properties.
pub fn write_info_object<W: Write + ?Sized>(out: &mut W, info: &OnStatusInfo) -> Result<()> {
    amf0::write_object_start(out)?;
    // tcUrl is listed first in the reconnect Info Object table.
    if let Some(tc_url) = &info.tc_url {
        amf0::write_property_name(out, "tcUrl")?;
        amf0::write_string(out, tc_url)?;
    }
    amf0::write_property_name(out, "code")?;
    amf0::write_string(out, &info.code)?;
    if let Some(description) = &info.description {
        amf0::write_property_name(out, "description")?;
        amf0::write_string(out, description)?;
    }
    amf0::write_property_name(out, "level")?;
    amf0::write_string(out, &info.level)?;
    for (name, value) in &info.extra {
        amf0::write_property_name(out, name)?;
        amf0::write_string(out, value)?;
    }
    amf0::write_object_end(out)?;
    Ok(())
}

/// Serialise the full `onStatus` command sequence into `out` (no RTMP
/// chunk framing): the four AMF0 values per the spec table — command
/// name `"onStatus"`, transaction id `0`, a `null` command object, and
/// the Info Object.
pub fn write_on_status_command_body<W: Write + ?Sized>(
    out: &mut W,
    info: &OnStatusInfo,
) -> Result<()> {
    amf0::write_string(out, COMMAND_NAME)?;
    // Transaction ID set to 0 (no response needed).
    amf0::write_number(out, 0.0)?;
    // Command Object: there is no command object for onStatus — null.
    amf0::write_null(out)?;
    write_info_object(out, info)?;
    Ok(())
}

/// Serialise the full `onStatus` command sequence to a fresh `Vec<u8>`.
pub fn write_on_status_command(info: &OnStatusInfo) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(128);
    write_on_status_command_body(&mut out, info)?;
    Ok(out)
}

/// Parse an `onStatus` command sequence back into a typed
/// [`OnStatusInfo`], closing the read↔write loop. Validates the command
/// name (`"onStatus"`), skips the transaction id and the `null` command
/// object, and reads the Info Object's string properties.
///
/// `code` and `level` default to empty strings when absent; the optional
/// `tcUrl` / `description` land in their `Option` fields; every other
/// string property is collected into [`OnStatusInfo::extra`] in
/// bitstream order. Non-string Info-Object values (the spec defines all
/// reconnect/status properties as strings) are ignored.
///
/// Returns [`Error::invalid`] if the byte stream does not begin with the
/// `"onStatus"` command name or does not carry an Object Info value.
pub fn parse_on_status_command(buf: &[u8]) -> Result<OnStatusInfo> {
    let (name, p) = parse_amf0_value(buf, 0)?;
    match name {
        AmfValue::String(s) if s == COMMAND_NAME => {}
        other => {
            return Err(Error::invalid(format!(
                "FLV onStatus: expected command name \"{COMMAND_NAME}\", got {other:?}"
            )));
        }
    }
    // Transaction ID (number) — skip.
    let (_txn, p) = parse_amf0_value(buf, p)?;
    // Command Object (null) — skip.
    let (_cmd_obj, p) = parse_amf0_value(buf, p)?;
    // Info Object.
    let (info_val, _p) = parse_amf0_value(buf, p)?;
    let props = match info_val {
        AmfValue::Object(props) | AmfValue::EcmaArray(props) => props,
        other => {
            return Err(Error::invalid(format!(
                "FLV onStatus: Info Object must be an AMF0 Object, got {other:?}"
            )));
        }
    };

    let mut info = OnStatusInfo::default();
    for (key, value) in props {
        let AmfValue::String(s) = value else {
            // Non-string Info Object properties are not part of the
            // reconnect/status shape — ignore them.
            continue;
        };
        match key.as_str() {
            "code" => info.code = s,
            "level" => info.level = s,
            "description" => info.description = Some(s),
            "tcUrl" => info.tc_url = Some(s),
            _ => info.extra.push((key, s)),
        }
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_request_fills_mandated_code_and_level() {
        let info = reconnect_request(Some("rtmp://foo.example/app"), None);
        assert_eq!(info.code, CODE_RECONNECT_REQUEST);
        assert_eq!(info.level, LEVEL_STATUS);
        assert_eq!(info.tc_url.as_deref(), Some("rtmp://foo.example/app"));
        assert!(info.description.is_none());
        assert!(info.is_reconnect_request());
    }

    #[test]
    fn command_body_starts_with_name_txn_null() {
        let info = reconnect_request(None, None);
        let bytes = write_on_status_command(&info).unwrap();
        // String marker + UI16 len(8) + "onStatus".
        assert_eq!(bytes[0], 0x02);
        assert_eq!(u16::from_be_bytes([bytes[1], bytes[2]]), 8);
        assert_eq!(&bytes[3..11], b"onStatus");
        // Number marker (transaction id 0.0).
        assert_eq!(bytes[11], 0x00);
        assert_eq!(&bytes[12..20], &0.0f64.to_be_bytes());
        // Null command object.
        assert_eq!(bytes[20], 0x05);
        // Info Object marker.
        assert_eq!(bytes[21], 0x03);
    }

    #[test]
    fn full_reconnect_round_trips() {
        let info = reconnect_request(
            Some("rtmp://127.0.0.1/realtimeapp"),
            Some("The streaming server is undergoing updates."),
        );
        let bytes = write_on_status_command(&info).unwrap();
        let back = parse_on_status_command(&bytes).unwrap();
        assert_eq!(back, info);
        assert!(back.is_reconnect_request());
    }

    #[test]
    fn base_status_without_optionals_round_trips() {
        let info = OnStatusInfo::new("NetConnection.Connect.Success", LEVEL_STATUS);
        let bytes = write_on_status_command(&info).unwrap();
        let back = parse_on_status_command(&bytes).unwrap();
        assert_eq!(back.code, "NetConnection.Connect.Success");
        assert_eq!(back.level, "status");
        assert!(back.description.is_none());
        assert!(back.tc_url.is_none());
        assert_eq!(back, info);
    }

    #[test]
    fn extra_properties_survive_round_trip_in_order() {
        let info = OnStatusInfo::new("NetConnection.Connect.Closed", LEVEL_STATUS)
            .description("closed cleanly")
            .property("clientId", "abc-123")
            .property("region", "us-east");
        let bytes = write_on_status_command(&info).unwrap();
        let back = parse_on_status_command(&bytes).unwrap();
        assert_eq!(
            back.extra,
            vec![
                ("clientId".to_string(), "abc-123".to_string()),
                ("region".to_string(), "us-east".to_string()),
            ]
        );
        assert_eq!(back, info);
    }

    #[test]
    fn property_order_in_body_is_tcurl_code_description_level() {
        let info = reconnect_request(Some("rtmp://s/app"), Some("d"));
        let mut body = Vec::new();
        write_info_object(&mut body, &info).unwrap();
        // Object start.
        assert_eq!(body[0], 0x03);
        // First property name is "tcUrl".
        let len = u16::from_be_bytes([body[1], body[2]]) as usize;
        assert_eq!(&body[3..3 + len], b"tcUrl");
    }

    #[test]
    fn parse_rejects_wrong_command_name() {
        // An onMetaData-style name should not parse as onStatus.
        let mut bytes = Vec::new();
        amf0::write_string(&mut bytes, "onMetaData").unwrap();
        amf0::write_number(&mut bytes, 0.0).unwrap();
        amf0::write_null(&mut bytes).unwrap();
        write_info_object(&mut bytes, &OnStatusInfo::new("x", "status")).unwrap();
        let err = parse_on_status_command(&bytes).unwrap_err();
        assert!(format!("{err}").contains("onStatus"));
    }

    #[test]
    fn parse_ignores_non_string_info_properties() {
        // Construct an Info Object with a numeric extra property.
        let mut bytes = Vec::new();
        amf0::write_string(&mut bytes, COMMAND_NAME).unwrap();
        amf0::write_number(&mut bytes, 0.0).unwrap();
        amf0::write_null(&mut bytes).unwrap();
        amf0::write_object_start(&mut bytes).unwrap();
        amf0::write_property_name(&mut bytes, "code").unwrap();
        amf0::write_string(&mut bytes, CODE_RECONNECT_REQUEST).unwrap();
        amf0::write_property_name(&mut bytes, "level").unwrap();
        amf0::write_string(&mut bytes, LEVEL_STATUS).unwrap();
        amf0::write_property_name(&mut bytes, "retryAfter").unwrap();
        amf0::write_number(&mut bytes, 5.0).unwrap();
        amf0::write_object_end(&mut bytes).unwrap();

        let back = parse_on_status_command(&bytes).unwrap();
        assert!(back.is_reconnect_request());
        // The numeric property was ignored, not collected as extra.
        assert!(back.extra.is_empty());
    }
}
