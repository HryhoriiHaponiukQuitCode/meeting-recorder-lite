//! Privacy permissions the user has to grant before recording (macOS). On
//! other systems there is nothing to ask and everything reports granted.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Permission {
    Microphone,
    /// System audio: "System Audio Recording" for the process tap (macOS
    /// 14.4+) or "Screen Recording" for ScreenCaptureKit (older macOS).
    SystemAudio,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Granted,
    Denied,
    /// Not asked yet: requesting shows the system prompt.
    NotAsked,
}

impl Permission {
    pub fn title(self) -> &'static str {
        match self {
            Permission::Microphone => "Мікрофон",
            Permission::SystemAudio => "Системний звук",
        }
    }
}

/// Permissions this system asks for.
pub fn needed() -> &'static [Permission] {
    if cfg!(target_os = "macos") {
        &[Permission::Microphone, Permission::SystemAudio]
    } else {
        &[]
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{Access, Permission};
    use recorder_capture::macos::{permissions as mac, process_tap};

    fn from(a: mac::Access, unknown: Access) -> Access {
        match a {
            mac::Access::Granted => Access::Granted,
            mac::Access::Denied => Access::Denied,
            mac::Access::Unknown => unknown,
        }
    }

    pub fn status(p: Permission) -> Access {
        match p {
            Permission::Microphone => from(mac::microphone_status(), Access::NotAsked),
            Permission::SystemAudio if process_tap::supported() => {
                from(mac::system_audio_status(), Access::NotAsked)
            }
            Permission::SystemAudio => from(mac::screen_capture_status(), Access::NotAsked),
        }
    }

    pub fn request(p: Permission) -> Access {
        match p {
            Permission::Microphone => from(mac::request_microphone(), Access::NotAsked),
            Permission::SystemAudio if process_tap::supported() => {
                from(mac::request_system_audio(), Access::NotAsked)
            }
            Permission::SystemAudio => from(mac::ensure_screen_capture(), Access::Denied),
        }
    }

    pub fn open_settings(p: Permission) {
        mac::open_privacy_settings(match p {
            Permission::Microphone => "Privacy_Microphone",
            Permission::SystemAudio => "Privacy_ScreenCapture",
        });
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::{Access, Permission};

    pub fn status(_: Permission) -> Access {
        Access::Granted
    }

    pub fn request(_: Permission) -> Access {
        Access::Granted
    }

    pub fn open_settings(_: Permission) {}
}

/// Current state, without asking.
pub fn status(p: Permission) -> Access {
    imp::status(p)
}

/// Shows the system prompt if the user has not decided yet and waits for the
/// answer. Blocks: call it off the UI thread.
pub fn request(p: Permission) -> Access {
    let answer = imp::request(p);
    tracing::info!(permission = ?p, ?answer, "permission requested");
    answer
}

/// Opens the privacy pane where a refused permission can be switched on.
pub fn open_settings(p: Permission) {
    imp::open_settings(p);
}
