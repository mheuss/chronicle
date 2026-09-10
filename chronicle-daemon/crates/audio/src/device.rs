//! Identity of the input device behind the microphone engine's input node.

use std::ptr;
use std::ptr::NonNull;

use objc2_avf_audio::AVAudioInputNode;
use objc2_core_audio::{
    AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID,
    AudioObjectPropertyAddress, AudioObjectPropertyScope, AudioObjectPropertySelector,
    kAudioAggregateDevicePropertyActiveSubDeviceList, kAudioDevicePropertyDeviceUID,
    kAudioDevicePropertyStreams, kAudioHardwarePropertyDefaultInputDevice,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeInput, kAudioObjectSystemObject, kAudioObjectUnknown,
};
use objc2_core_foundation::{CFRetained, CFString};

/// The identity of the input device the line names.
///
/// Not necessarily the device the node is bound to — see `describe`. Both
/// fields are `Option` because the two property reads fail independently; a UID
/// with no name still separates two same-named devices.
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

/// Renders the device fields for the log line.
///
/// A function rather than a `Display` impl: the `unknown` substitution is this
/// line's policy, not the type's identity.
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

/// How many bytes a property currently occupies, or `None` if the read failed.
///
/// A device that is not an aggregate has no sub-device list, and this read is
/// where that shows up. An unreadable property and an absent one both land on
/// `None`, so the `OSStatus` is logged to tell them apart.
fn property_size(
    device: AudioObjectID,
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
    stage: &str,
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
    if status != 0 {
        log::debug!("device lookup: {stage} size unavailable, OSStatus {status}");
        return None;
    }
    Some(size as usize)
}

/// The sub-devices of an aggregate, paired with each one's input stream count.
///
/// `None` means the device has no sub-device list property at all, so it is not
/// an aggregate and is already the one to name. `Some` of an empty list means it
/// *is* an aggregate but nothing usable came back — a different situation, and
/// naming that device would put `CADefaultDeviceAggregate-<pid>-0` in the line.
fn subdevices_with_input_counts(device: AudioObjectID) -> Option<Vec<(AudioObjectID, usize)>> {
    // `?` exits when no list came back — normally because the device is not an
    // aggregate, though `property_size` cannot tell that from a read that
    // failed for another reason.
    let bytes = property_size(
        device,
        kAudioAggregateDevicePropertyActiveSubDeviceList,
        kAudioObjectPropertyScopeGlobal,
        "sub-device list",
    )?;

    let Some(count) = elements_to_allocate(bytes) else {
        // Zero is a real state — an aggregate whose members are all unplugged
        // reports no *active* ones — so this is not necessarily a fault.
        log::debug!("device lookup: sub-device list reported {bytes} bytes, not a usable count");
        return Some(Vec::new());
    };

    let address = AudioObjectPropertyAddress {
        mSelector: kAudioAggregateDevicePropertyActiveSubDeviceList,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut ids = vec![kAudioObjectUnknown; count];
    // Not `bytes`: `count` rounded it down to whole ids, so a non-multiple
    // would tell CoreAudio the buffer is larger than it is.
    let buffer_bytes = count * size_of::<AudioObjectID>();
    let mut size = buffer_bytes as u32;

    // SAFETY: `ids` holds `count` ids and `size` is that same byte count, so the
    // out buffer matches what CoreAudio was told it has. `NonNull::from` on the
    // slice needs no null justification of its own. The qualifier is empty,
    // which this selector permits.
    let status = unsafe {
        AudioObjectGetPropertyData(
            device,
            NonNull::from(&address),
            0,
            ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(ids.as_mut_slice()).cast(),
        )
    };
    if status != 0 {
        log::debug!("device lookup: sub-device list read failed, OSStatus {status}");
        return Some(Vec::new());
    }

    let Some(keep) = elements_to_keep(buffer_bytes, size as usize) else {
        log::debug!(
            "device lookup: sub-device list reported {size} bytes into a {buffer_bytes}-byte buffer"
        );
        return Some(Vec::new());
    };
    ids.truncate(keep);

    let counts = ids
        .into_iter()
        .map(|id| {
            // A device with no input reports a readable size of 0, so `None`
            // here is a failed read rather than an absent input.
            let bytes = property_size(
                id,
                kAudioDevicePropertyStreams,
                kAudioObjectPropertyScopeInput,
                "input stream count",
            )
            .unwrap_or(0);
            (id, bytes / size_of::<AudioObjectID>())
        })
        .collect();
    Some(counts)
}

/// A bound on the member count, which comes from the HAL handler — third-party
/// code for a virtual device — and sizes an allocation. 64 is arbitrary: Apple
/// documents no limit, and it is far above anything observed.
const MAX_SUBDEVICES: usize = 64;

/// How many ids to allocate for a sub-device list of `bytes`, or `None` when
/// that count is implausible. Unbounded, it would size an allocation from a
/// number the handler chose, and a failed allocation aborts rather than
/// unwinding.
fn elements_to_allocate(bytes: usize) -> Option<usize> {
    let count = bytes / size_of::<AudioObjectID>();
    (1..=MAX_SUBDEVICES).contains(&count).then_some(count)
}

/// How many ids to keep after CoreAudio reports writing `reported_bytes` into a
/// buffer of `buffer_bytes`. Shrinking is legitimate — the route can change
/// between the two calls. Growing means the write overran the buffer, so `None`
/// rejects the read rather than trusting part of it.
fn elements_to_keep(buffer_bytes: usize, reported_bytes: usize) -> Option<usize> {
    (reported_bytes <= buffer_bytes).then(|| reported_bytes / size_of::<AudioObjectID>())
}

/// The device the user selected as the system default input, if it reads.
fn default_input_device() -> Option<AudioObjectID> {
    let address = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyDefaultInputDevice,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut device = kAudioObjectUnknown;
    let mut size = size_of::<AudioObjectID>() as u32;

    // SAFETY: `address`, `size` and `device` are live locals, and `device` is an
    // `AudioObjectID` slot sized to match `size` — which is what this selector
    // returns. The qualifier is empty, which it permits.
    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject as AudioObjectID,
            NonNull::from(&address),
            0,
            ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut device).cast(),
        )
    };
    if status != 0 {
        log::debug!("device lookup: default input read failed, OSStatus {status}");
        return None;
    }
    // Same in/out size check the other reads carry. This handler is Apple's
    // rather than a HAL plugin's, so a short write is far-fetched — but a
    // partial one would leave a garbage id that passes the filter below.
    if size as usize != size_of::<AudioObjectID>() {
        log::debug!("device lookup: default input reported {size} bytes, expected an id");
        return None;
    }
    (device != kAudioObjectUnknown).then_some(device)
}

/// Which device the line should name, given the member resolved out of the
/// aggregate and the user's selected default input.
///
/// Normally the same object, and a no-op. They diverge when the selection is
/// itself an aggregate: CoreAudio flattens it into the engine's, so the members
/// are physical devices and the selection is absent from the list. Naming a
/// member there reports a device the user did not choose.
fn device_to_report(
    resolved: AudioObjectID,
    default_input: Option<AudioObjectID>,
) -> AudioObjectID {
    match default_input {
        Some(default) if default != resolved => default,
        _ => resolved,
    }
}

/// Picks the sub-device that carries the input.
///
/// A default-device aggregate wraps both halves of the route, so the output
/// device is in the list too; input stream count separates them. With more than
/// one input member this takes the first — a fallback, not a decision, since
/// `ActiveSubDeviceList` has no documented ordering.
fn first_input_subdevice(subdevices: &[(AudioObjectID, usize)]) -> Option<AudioObjectID> {
    subdevices
        .iter()
        .find(|(_, input_streams)| *input_streams > 0)
        .map(|(id, _)| *id)
}

/// The identity of the input device the tap will capture from.
///
/// The node is often bound to an aggregate wrapping the default route, so this
/// descends to the member carrying input, then cross-checks that member against
/// the selected default input and prefers the selection when they differ. A
/// device bound directly is reported as-is.
///
/// An unreadable field renders as `unknown` rather than failing the caller. The
/// one way this does not return is the nil-`AUAudioUnit` panic noted below,
/// which the header's `NS_ASSUME_NONNULL` contract rules out.
///
/// The system-default read is the cross-check only — Architectural Decision 7
/// forbids falling back to it when the bound-device read *fails*, and that path
/// returns `unknown`.
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

    // The node is usually bound to an aggregate wrapping the default route
    // rather than to the microphone. Its name and UID are both
    // `CADefaultDeviceAggregate-<pid>-0` — the trailing number is the process
    // id — so reporting it names no device and changes every run.
    let device = match subdevices_with_input_counts(device) {
        // An ordinary path, not a failure: after a route change the node binds
        // straight to a device, which is then already the one to name.
        None => {
            log::debug!("device lookup: {device} has no sub-device list, naming it as-is");
            device
        }
        Some(subdevices) => match first_input_subdevice(&subdevices) {
            Some(input) => {
                let report = device_to_report(input, default_input_device());
                if report == input {
                    log::debug!(
                        "device lookup: resolved aggregate {device} to input sub-device {input}"
                    );
                } else {
                    // The selected default is itself an aggregate, flattened
                    // into this one — naming a member would report a device the
                    // user did not choose.
                    log::debug!(
                        "device lookup: aggregate {device} flattens the selected default {report}; naming the selection, not member {input}"
                    );
                }
                report
            }
            None => {
                // Reporting the aggregate here would put
                // `CADefaultDeviceAggregate-<pid>-0` in the line, which names
                // nothing. `unknown` at least says so. Reached when the list is
                // present but empty too — an aggregate whose members are all
                // unplugged reports no *active* ones.
                log::debug!(
                    "device lookup: no input among {} member(s) of aggregate {device}",
                    subdevices.len()
                );
                return absent;
            }
        },
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
    fn an_empty_list_allocates_nothing() {
        assert_eq!(elements_to_allocate(0), None);
    }

    #[test]
    fn a_partial_element_rounds_down_and_is_rejected() {
        // Three bytes is not one id. Rounding down gives zero, which is not a
        // plausible list.
        assert_eq!(elements_to_allocate(3), None);
    }

    #[test]
    fn a_single_member_list_allocates_one() {
        // Pins the floor. Without this, widening the guard to `2..=MAX` passes
        // every other test — and one member is the case that decides whether a
        // normal aggregate resolves at all.
        assert_eq!(elements_to_allocate(size_of::<AudioObjectID>()), Some(1));
    }

    #[test]
    fn a_normal_list_allocates_its_element_count() {
        assert_eq!(
            elements_to_allocate(4 * size_of::<AudioObjectID>()),
            Some(4)
        );
    }

    #[test]
    fn the_ceiling_is_inclusive() {
        assert_eq!(
            elements_to_allocate(MAX_SUBDEVICES * size_of::<AudioObjectID>()),
            Some(MAX_SUBDEVICES)
        );
    }

    #[test]
    fn an_implausible_count_allocates_nothing() {
        // The byte count comes from the HAL handler and sizes an allocation. A
        // failed allocation aborts rather than unwinding, which would take the
        // daemon down.
        assert_eq!(
            elements_to_allocate((MAX_SUBDEVICES + 1) * size_of::<AudioObjectID>()),
            None
        );
    }

    #[test]
    fn keeping_an_exact_report_keeps_everything() {
        let bytes = 2 * size_of::<AudioObjectID>();
        assert_eq!(elements_to_keep(bytes, bytes), Some(2));
    }

    #[test]
    fn a_shrunk_report_keeps_only_what_was_written() {
        // The route can change between the size call and the data call.
        assert_eq!(
            elements_to_keep(
                4 * size_of::<AudioObjectID>(),
                2 * size_of::<AudioObjectID>()
            ),
            Some(2)
        );
    }

    #[test]
    fn a_report_of_nothing_keeps_nothing() {
        assert_eq!(elements_to_keep(4 * size_of::<AudioObjectID>(), 0), Some(0));
    }

    #[test]
    fn an_over_report_rejects_the_whole_read() {
        // Growing means the write overran the buffer it was handed, so nothing
        // in it can be trusted — not even the prefix.
        assert_eq!(
            elements_to_keep(
                2 * size_of::<AudioObjectID>(),
                3 * size_of::<AudioObjectID>()
            ),
            None
        );
    }

    #[test]
    fn a_partial_trailing_element_is_dropped() {
        let bytes = 4 * size_of::<AudioObjectID>();
        assert_eq!(elements_to_keep(bytes, bytes - 1), Some(3));
    }

    #[test]
    fn the_resolved_member_is_named_when_it_is_the_selected_default() {
        // The ordinary case: the engine's aggregate wraps exactly the device
        // the user picked, so resolving lands on it.
        assert_eq!(device_to_report(131, Some(131)), 131);
    }

    #[test]
    fn a_flattened_selection_is_named_over_its_member() {
        // Reproduced on hardware: selecting a user-built aggregate makes
        // CoreAudio flatten it into the engine's aggregate, so the members are
        // physical devices and the selection is absent from the list. Naming
        // member 117 there produced a line byte-identical to selecting 117
        // directly.
        assert_eq!(device_to_report(117, Some(173)), 173);
    }

    #[test]
    fn an_unreadable_default_leaves_the_resolved_member() {
        // Better to name the member than to degrade to `unknown` over a
        // cross-check that did not read.
        assert_eq!(device_to_report(117, None), 117);
    }

    #[test]
    fn no_subdevices_resolves_to_nothing() {
        assert_eq!(first_input_subdevice(&[]), None);
    }

    #[test]
    fn a_list_with_no_input_resolves_to_nothing() {
        // An output-only aggregate has no microphone to name. `describe`
        // renders `unknown` rather than naming a speaker.
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
        // Pins the fallback, not a claim that first is correct. See the doc
        // comment: with no documented ordering there is nothing better to pick.
        assert_eq!(first_input_subdevice(&[(131, 2), (117, 1)]), Some(131));
    }
}
