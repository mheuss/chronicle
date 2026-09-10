//! Identity of the audio device the microphone engine is bound to.

/// The bound input device's identity, as far as CoreAudio would report it.
///
/// Both fields are `Option` because the two property reads fail independently.
/// A UID with no name still separates two same-named devices.
pub(crate) struct InputDevice {
    pub(crate) name: Option<String>,
    pub(crate) uid: Option<String>,
}

/// Escapes a device string so one log record stays on one line.
///
/// Device names are user-editable in Audio MIDI Setup and UIDs are opaque, so
/// neither is safe to interpolate raw. Per Architectural Decision 8.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str(r"\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str(r"\n"),
            '\r' => out.push_str(r"\r"),
            '\t' => out.push_str(r"\t"),
            c if c.is_control() => out.push_str(&format!("\\u{{{:04x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

pub(crate) fn render_fields(device: &InputDevice) -> String {
    let name = match device.name.as_deref() {
        Some(value) => escape(value),
        None => "unknown".to_string(),
    };
    let uid = match device.uid.as_deref() {
        Some(value) => escape(value),
        None => "unknown".to_string(),
    };
    format!("device_name=\"{name}\" device_uid=\"{uid}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(name: Option<&str>, uid: Option<&str>) -> InputDevice {
        InputDevice {
            name: name.map(str::to_string),
            uid: uid.map(str::to_string),
        }
    }

    #[test]
    fn renders_both_fields_when_both_are_present() {
        assert_eq!(
            render_fields(&device(Some("Yeti Stereo Microphone"), Some("UID-1"))),
            r#"device_name="Yeti Stereo Microphone" device_uid="UID-1""#
        );
    }

    #[test]
    fn renders_unknown_for_a_missing_name_only() {
        // Separate from the uid arm below: one combined test passes with
        // either field hard-coded.
        assert_eq!(
            render_fields(&device(None, Some("UID-1"))),
            r#"device_name="unknown" device_uid="UID-1""#
        );
    }

    #[test]
    fn renders_unknown_for_a_missing_uid_only() {
        assert_eq!(
            render_fields(&device(Some("Yeti"), None)),
            r#"device_name="Yeti" device_uid="unknown""#
        );
    }

    #[test]
    fn renders_both_fields_when_neither_is_present() {
        assert_eq!(
            render_fields(&device(None, None)),
            r#"device_name="unknown" device_uid="unknown""#
        );
    }

    #[test]
    fn escapes_a_quote_in_a_device_name() {
        assert_eq!(
            render_fields(&device(Some(r#"a"b"#), None)),
            r#"device_name="a\"b" device_uid="unknown""#
        );
    }

    #[test]
    fn escapes_a_backslash_once() {
        assert_eq!(
            render_fields(&device(Some(r"a\b"), None)),
            r#"device_name="a\\b" device_uid="unknown""#
        );
    }

    #[test]
    fn a_name_with_newlines_renders_on_one_line() {
        let rendered = render_fields(&device(Some("a\r\nb"), None));
        assert!(!rendered.contains('\n'), "rendered: {rendered}");
        assert!(!rendered.contains('\r'), "rendered: {rendered}");
        assert_eq!(rendered, r#"device_name="a\r\nb" device_uid="unknown""#);
    }

    #[test]
    fn an_empty_name_is_distinct_from_a_missing_one() {
        assert_eq!(
            render_fields(&device(Some(""), None)),
            r#"device_name="" device_uid="unknown""#
        );
    }

    #[test]
    fn escapes_a_tab_in_a_device_name() {
        // Pins the `\t` arm specifically. Without this, deleting that arm
        // still passes: tab falls through to the control-character arm and
        // renders as `\u{0009}`, which nothing asserts on.
        assert_eq!(
            render_fields(&device(Some("a\tb"), None)),
            r#"device_name="a\tb" device_uid="unknown""#
        );
    }

    #[test]
    fn escapes_a_control_character_outside_the_named_set() {
        // BR-5 covers control characters generally, not just the four with
        // their own arms. This is the only test holding the catch-all arm in
        // place.
        assert_eq!(
            render_fields(&device(Some("a\u{1}b"), None)),
            r#"device_name="a\u{0001}b" device_uid="unknown""#
        );
    }

    #[test]
    fn escapes_the_uid_field_too() {
        // Every other escaping test leaves uid `None`, so the uid arm's call
        // to `escape` is otherwise unexercised — dropping it would pass.
        assert_eq!(
            render_fields(&device(None, Some(r#"a"b"#))),
            r#"device_name="unknown" device_uid="a\"b""#
        );
    }
}
