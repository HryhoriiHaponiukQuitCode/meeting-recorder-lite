//! Information about the current output device, used to judge whether the
//! microphone can hear the remote side (speakers instead of headphones).

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{DeviceType, InterfaceType};

#[derive(Clone, Debug)]
pub struct OutputInfo {
    pub name: Option<String>,
    /// `speakers`, `headphones`, `headset`, `bluetooth` or `unknown`.
    pub kind: &'static str,
    /// `likely`, `unlikely` or `unknown`.
    pub echo_risk: &'static str,
}

pub fn default_output() -> OutputInfo {
    let host = cpal::default_host();
    let Some(desc) = host
        .default_output_device()
        .and_then(|d| d.description().ok())
    else {
        return OutputInfo {
            name: None,
            kind: "unknown",
            echo_risk: "unknown",
        };
    };
    let name = desc.name().to_lowercase();
    let named = |words: &[&str]| words.iter().any(|w| name.contains(w));
    let kind = match (desc.device_type(), desc.interface_type()) {
        (DeviceType::Headphones | DeviceType::Earpiece, _) => "headphones",
        (DeviceType::Headset, _) => "headset",
        (_, InterfaceType::Bluetooth) => "bluetooth",
        (DeviceType::Speaker, _) => "speakers",
        // Hosts that do not classify devices: fall back to the name.
        _ if named(&["headphone", "headset", "airpods", "навушник", "гарнітур"]) => {
            "headphones"
        }
        _ if named(&["speaker", "динамік", "колонк"]) => "speakers",
        (_, InterfaceType::BuiltIn) => "speakers",
        _ => "unknown",
    };
    let echo_risk = match kind {
        "speakers" => "likely",
        "headphones" | "headset" | "bluetooth" => "unlikely",
        _ => "unknown",
    };
    OutputInfo {
        name: Some(desc.name().to_string()),
        kind,
        echo_risk,
    }
}

/// Names of the available output devices (for `recorder devices`).
pub fn list_outputs() -> Vec<String> {
    let host = cpal::default_host();
    host.output_devices()
        .map(|it| it.map(|d| crate::mic::device_name(&d)).collect())
        .unwrap_or_default()
}
