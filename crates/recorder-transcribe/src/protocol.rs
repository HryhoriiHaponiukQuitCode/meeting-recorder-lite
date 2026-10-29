//! Progress lines the transcriber prints on stdout with `--json`, one JSON
//! object per line, read by the window and the CLI.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Message {
    /// The model is loaded; `audio_s` seconds of recording to process.
    Started {
        audio_s: f64,
    },
    /// `done_s` of `total_s` seconds of audio processed.
    Progress {
        done_s: f64,
        total_s: f64,
    },
    /// Finished; files written in the session folder.
    Done {
        files: Vec<String>,
        processing_s: f64,
    },
    Error {
        message: String,
    },
}

impl Message {
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).expect("serializable")
    }

    /// `None` for lines that are not protocol messages (e.g. engine logs).
    pub fn parse(line: &str) -> Option<Self> {
        serde_json::from_str(line.trim()).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_round_trip_and_noise_is_ignored() {
        let m = Message::Progress {
            done_s: 12.5,
            total_s: 60.0,
        };
        let line = m.to_line();
        assert_eq!(line, r#"{"event":"progress","done_s":12.5,"total_s":60.0}"#);
        assert_eq!(Message::parse(&line), Some(m));
        assert_eq!(Message::parse("some engine log line"), None);
    }
}
