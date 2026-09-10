//! Identity of the audio device the microphone engine is bound to.

use std::ptr;
use std::ptr::NonNull;

use objc2_avf_audio::AVAudioInputNode;
use objc2_core_audio::{
    AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID,
    AudioObjectPropertyAddress, AudioObjectPropertyScope, AudioObjectPropertySelector,
    kAudioAggregateDevicePropertyActiveSubDeviceList, kAudioDevicePropertyDeviceUID,
    kAudioDevicePropertyStreams, kAudioObjectPropertyElementMain, kAudioObjectPropertyName,
    kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeInput, kAudioObjectUnknown,
};
use objc2_core_foundation::{CFRetained, CFString};

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

/// Reads one CFString device property.
///
/// `kAudioObjectPropertyName` and `kAudioDevicePropertyDeviceUID` both return a
/// +1 reference: `AudioHardwareBase.h` documents each as "The caller is
/// responsible for releasing the returned CFObject", unlike a typical CF Get
/// function. `CFRetained::from_raw` takes that ownership and releases on drop,
/// so the string is copied out before it falls.
///
/// Safe to call with any id. A stale or invalid id is not a Rust safety
/// violation — CoreAudio answers with `kAudioHardwareBadObjectError`, which
/// becomes `None` here. Only the FFI call itself is `unsafe`.
fn copy_string_property(
    device: AudioObjectID,
    selector: AudioObjectPropertySelector,
    stage: &str,
) -> Option<String> {
    let address = AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };

    let mut out: *const CFString = ptr::null();
    let mut size = size_of::<*const CFString>() as u32;

    // SAFETY: `address` and `size` are live locals; `out` is a live pointer
    // slot sized to match `size`, and both selectors return a `CFStringRef`, so
    // what CoreAudio writes there is a valid `*const CFString`. The qualifier is
    // empty, which these selectors permit.
    let status = unsafe {
        AudioObjectGetPropertyData(
            device,
            NonNull::from(&address),
            0,
            ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut out).cast(),
        )
    };
    if status != 0 {
        log::debug!("device lookup: {stage} read failed, OSStatus {status}");
        return None;
    }

    // `size` is in/out: CoreAudio overwrites it with what the handler actually
    // wrote. A handler that writes fewer bytes than the slot holds leaves `out`
    // non-null but half-initialised, which would reach `from_raw` below and
    // release garbage. Device property handlers belong to the HAL plugin, which
    // for a virtual device is third-party code.
    if size as usize != size_of::<*const CFString>() {
        log::debug!("device lookup: {stage} reported {size} bytes, expected a CFStringRef");
        return None;
    }

    let Some(ptr) = NonNull::new(out.cast_mut()) else {
        log::debug!("device lookup: {stage} returned null with OSStatus 0");
        return None;
    };
    // SAFETY: the call succeeded and the pointer is non-null, so `ptr` is a
    // +1 CFString this scope owns.
    Some(unsafe { CFRetained::from_raw(ptr) }.to_string())
}

/// How many bytes a property currently occupies, or `None` if it is absent.
///
/// A non-aggregate device has no sub-device list at all, so the size read
/// itself fails. That is the signal the bound device needs no resolving, not an
/// error worth surfacing.
fn property_size(
    device: AudioObjectID,
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> Option<usize> {
    let address = AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut size = 0u32;
    // SAFETY: `address` and `size` are live locals, and the qualifier is empty,
    // which these selectors permit. This call only measures — it writes nothing
    // into a data buffer.
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            device,
            NonNull::from(&address),
            0,
            ptr::null(),
            NonNull::from(&mut size),
        )
    };
    (status == 0).then_some(size as usize)
}

/// The sub-devices of an aggregate, paired with each one's input stream count.
///
/// Empty for a device that is not an aggregate.
fn subdevices_with_input_counts(device: AudioObjectID) -> Vec<(AudioObjectID, usize)> {
    let Some(bytes) = property_size(
        device,
        kAudioAggregateDevicePropertyActiveSubDeviceList,
        kAudioObjectPropertyScopeGlobal,
    ) else {
        return Vec::new();
    };

    let count = bytes / size_of::<AudioObjectID>();
    if count == 0 {
        return Vec::new();
    }

    let address = AudioObjectPropertyAddress {
        mSelector: kAudioAggregateDevicePropertyActiveSubDeviceList,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut ids = vec![kAudioObjectUnknown; count];
    let mut size = bytes as u32;

    // SAFETY: `ids` holds `count` ids and `size` is that same byte count, so the
    // out buffer matches what CoreAudio was told it has. The qualifier is empty,
    // which this selector permits.
    let status = unsafe {
        AudioObjectGetPropertyData(
            device,
            NonNull::from(&address),
            0,
            ptr::null(),
            NonNull::from(&mut size),
            NonNull::new_unchecked(ids.as_mut_ptr()).cast(),
        )
    };
    if status != 0 {
        log::debug!("device lookup: sub-device list read failed, OSStatus {status}");
        return Vec::new();
    }

    // The list can shrink between the two calls if the route changes underneath
    // us. Trust what this call reported, not what the first one did.
    ids.truncate(size as usize / size_of::<AudioObjectID>());

    ids.into_iter()
        .map(|id| {
            let bytes = property_size(
                id,
                kAudioDevicePropertyStreams,
                kAudioObjectPropertyScopeInput,
            )
            .unwrap_or(0);
            (id, bytes / size_of::<AudioObjectID>())
        })
        .collect()
}

/// Picks the sub-device that carries the input.
///
/// A default-device aggregate wraps both halves of the default route, so the
/// output device is in the list too. Input stream count is what separates them.
fn first_input_subdevice(subdevices: &[(AudioObjectID, usize)]) -> Option<AudioObjectID> {
    subdevices
        .iter()
        .find(|(_, input_streams)| *input_streams > 0)
        .map(|(id, _)| *id)
}

/// The identity of the device the engine's input node is bound to.
///
/// Never fails. Any unreadable field comes back `None` and renders as
/// `unknown`. Per Architectural Decision 7, this does not fall back to the
/// system default input device.
pub(crate) fn describe(node: &AVAudioInputNode) -> InputDevice {
    let absent = InputDevice {
        name: None,
        uid: None,
    };

    // SAFETY: `AVAudioNode.h` declares the `AUAudioUnit` property non-nullable
    // inside `NS_ASSUME_NONNULL`, so the send cannot return nil. That matters
    // because objc2 panics on a nil return here rather than handing back a null
    // `Retained` — it is the one path on which this function would not return.
    let unit = unsafe { node.AUAudioUnit() };
    // SAFETY: `deviceID` hands back a plain `AUAudioObjectID` with no pointer
    // obligations. Its only precondition is a receiver of the right class, which
    // the typed `Retained<AUAudioUnit>` above supplies.
    let device = unsafe { unit.deviceID() };
    if device == kAudioObjectUnknown {
        log::debug!("device lookup: the input node reports no bound device");
        return absent;
    }

    // `AVAudioEngine` binds to a private aggregate wrapping the default route,
    // not to the microphone. Its name is synthetic (`CADefaultDeviceAggregate-
    // <pid>-0`) and changes every run, so reporting it would defeat the point.
    // A device that is not an aggregate has no sub-device list and falls
    // through unchanged.
    let device = match first_input_subdevice(&subdevices_with_input_counts(device)) {
        Some(input) => {
            log::debug!("device lookup: resolved aggregate {device} to input sub-device {input}");
            input
        }
        None => device,
    };

    InputDevice {
        name: copy_string_property(device, kAudioObjectPropertyName, "name"),
        uid: copy_string_property(device, kAudioDevicePropertyDeviceUID, "uid"),
    }
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
    #[test]
    fn no_subdevices_resolves_to_nothing() {
        assert_eq!(first_input_subdevice(&[]), None);
    }

    #[test]
    fn a_list_with_no_input_resolves_to_nothing() {
        // An output-only aggregate is not the microphone's, so falling back to
        // the bound device is better than naming a speaker.
        assert_eq!(first_input_subdevice(&[(105, 0), (67, 0)]), None);
    }

    #[test]
    fn the_input_subdevice_wins_over_the_output_half() {
        // The real shape observed on this machine: the mic carries one input
        // stream, the speakers carry none.
        assert_eq!(first_input_subdevice(&[(117, 1), (105, 0)]), Some(117));
    }

    #[test]
    fn the_input_subdevice_is_found_when_it_is_not_first() {
        // Ordering is not documented, so a test that only ever puts the input
        // first would pass on an implementation that returns element zero.
        assert_eq!(first_input_subdevice(&[(105, 0), (117, 1)]), Some(117));
    }

    #[test]
    fn the_first_of_several_inputs_wins() {
        assert_eq!(first_input_subdevice(&[(131, 2), (117, 1)]), Some(131));
    }
}
