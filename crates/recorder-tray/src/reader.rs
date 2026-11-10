//! The transcript pane: the dialogue of the selected recording, read from
//! its `transcript.json` and shown as a document (times, names in each
//! person's colour, selectable text), with a search that highlights.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use eframe::egui::{
    self, Align, Color32, FontId, Layout, RichText, TextFormat, text::LayoutJob, vec2,
};
use recorder_transcribe::output::{Document, JSON_FILE, clock};
use recorder_transcribe::{Speaker, dialog};

use crate::ui;

/// Ink for each participant: the microphone always the brand blue (as the
/// "You" tile), the others in order of appearance. Deep enough to read on
/// white, distinct from each other.
const PEOPLE: [Color32; 8] = [
    Color32::from_rgb(0x17, 0x68, 0xd8),
    Color32::from_rgb(0x0f, 0x7a, 0x55),
    Color32::from_rgb(0xa1, 0x56, 0x00),
    Color32::from_rgb(0x7b, 0x3f, 0xb8),
    Color32::from_rgb(0xb8, 0x2b, 0x5a),
    Color32::from_rgb(0x0e, 0x76, 0x8c),
    Color32::from_rgb(0x6b, 0x6e, 0x12),
    Color32::from_rgb(0x8a, 0x4b, 0x2a),
];
/// Background of a search match.
const MATCH: Color32 = Color32::from_rgb(0xff, 0xe7, 0x9a);
/// Width of the time column.
const TIME_W: f32 = 64.0;

pub struct Person {
    pub label: String,
    pub color: Color32,
    /// Channel and voice, which identify them in `transcript.json`.
    pub key: (Speaker, Option<u32>),
}

/// A participant's name being edited in the participants line.
pub struct PersonEdit {
    pub key: (Speaker, Option<u32>),
    pub text: String,
    /// The label before (the field's hint).
    pub was: String,
    focus: bool,
}

pub struct Line {
    pub start: f64,
    pub who: usize,
    pub text: String,
    /// Lower-case characters of `text`, one per character, for search.
    lower: Vec<char>,
}

pub struct Transcript {
    pub people: Vec<Person>,
    pub lines: Vec<Line>,
}

#[derive(Default)]
pub struct Reader {
    dir: Option<PathBuf>,
    modified: Option<SystemTime>,
    /// `None`: no transcript file; `Err`: it could not be read.
    pub transcript: Option<Result<Transcript, String>>,
    pub query: String,
}

impl Reader {
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// Shows another recording (a new search, the top of the text).
    pub fn select(&mut self, dir: Option<PathBuf>) {
        if dir != self.dir {
            self.dir = dir;
            self.modified = None;
            self.transcript = None;
            self.query.clear();
            self.refresh();
        }
    }

    /// Reads the transcript again now (after it was changed here).
    pub fn reload(&mut self) {
        self.modified = None;
        self.refresh();
    }

    /// Re-reads the transcript when its file changed on disk.
    pub fn refresh(&mut self) {
        let Some(dir) = &self.dir else {
            self.transcript = None;
            return;
        };
        let path = dir.join(JSON_FILE);
        let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if modified.is_none() {
            self.modified = None;
            self.transcript = None;
            return;
        }
        if modified != self.modified {
            self.modified = modified;
            self.transcript = Some(load(&path));
        }
    }
}

fn lower_chars(s: &str) -> Vec<char> {
    s.chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect()
}

fn load(path: &Path) -> Result<Transcript, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let doc: Document =
        serde_json::from_slice(&bytes).map_err(|e| format!("cannot read the transcript: {e}"))?;
    let turns = dialog::turns(&doc.utterances);
    // People in order of appearance; the microphone first in colour.
    let mut keys: Vec<(Speaker, Option<u32>)> = Vec::new();
    for t in &turns {
        if !keys.contains(&(t.speaker, t.voice)) {
            keys.push((t.speaker, t.voice));
        }
    }
    let mut next_color = 1;
    let people: Vec<Person> = keys
        .iter()
        .map(|&(speaker, voice)| {
            // Blue for the first voice on the microphone (the user, as a
            // rule); others on it take the next colours.
            let mic = speaker == Speaker::Me
                && keys.iter().find(|k| k.0 == Speaker::Me) == Some(&(speaker, voice));
            let color = if mic {
                PEOPLE[0]
            } else {
                next_color += 1;
                PEOPLE[1 + (next_color - 2) % (PEOPLE.len() - 1)]
            };
            Person {
                label: doc.label(speaker, voice),
                color,
                key: (speaker, voice),
            }
        })
        .collect();
    let lines = turns
        .into_iter()
        .map(|t| {
            let who = keys
                .iter()
                .position(|k| *k == (t.speaker, t.voice))
                .unwrap_or(0);
            Line {
                start: t.start,
                who,
                lower: lower_chars(&t.text),
                text: t.text,
            }
        })
        .collect();
    Ok(Transcript { people, lines })
}

/// Character ranges of `query` in `line` (case-insensitive).
fn matches(line: &Line, query: &[char]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    if query.is_empty() || query.len() > line.lower.len() {
        return out;
    }
    let mut i = 0;
    while i + query.len() <= line.lower.len() {
        if line.lower[i..i + query.len()] == *query {
            out.push((i, i + query.len()));
            i += query.len();
        } else {
            i += 1;
        }
    }
    out
}

/// The line's text with the matches highlighted.
fn text_job(line: &Line, found: &[(usize, usize)], width: f32) -> LayoutJob {
    let plain = TextFormat {
        font_id: FontId::proportional(14.5),
        color: ui::INK,
        line_height: Some(21.0),
        ..Default::default()
    };
    let hit = TextFormat {
        background: MATCH,
        ..plain.clone()
    };
    let mut job = LayoutJob::default();
    job.wrap.max_width = width;
    // Character positions to byte offsets.
    let bytes: Vec<usize> = line
        .text
        .char_indices()
        .map(|(b, _)| b)
        .chain(std::iter::once(line.text.len()))
        .collect();
    let mut at = 0;
    for &(a, b) in found {
        job.append(&line.text[bytes[at]..bytes[a]], 0.0, plain.clone());
        job.append(&line.text[bytes[a]..bytes[b]], 0.0, hit.clone());
        at = b;
    }
    job.append(&line.text[bytes[at]..], 0.0, plain);
    job
}

/// Everyone in the call, in one quiet line: "Participants  Григорій ·
/// Катя · Роман". A click on a name opens a field to rename them (when
/// `editable`); returns the participant and name to save.
pub fn people(
    ui: &mut egui::Ui,
    t: &Transcript,
    edit: &mut Option<PersonEdit>,
    editable: bool,
) -> Option<((Speaker, Option<u32>), String)> {
    let caption = |ui: &mut egui::Ui| {
        ui.label(
            RichText::new("Participants")
                .font(FontId::new(13.0, ui::medium()))
                .color(ui::INK_MUTED),
        );
    };
    if let Some(e) = edit.as_mut() {
        let (mut save, mut cancel) = (false, false);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = ui::S2;
            caption(ui);
            let field = ui.add(
                egui::TextEdit::singleline(&mut e.text)
                    .hint_text(e.was.as_str())
                    .char_limit(recorder_transcribe::output::NAME_MAX_CHARS)
                    .desired_width(220.0)
                    .margin(vec2(8.0, 5.0)),
            );
            if e.focus {
                field.request_focus();
                e.focus = false;
            }
            if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                save = true;
            }
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                cancel = true;
            }
            save |= ui::primary_small(ui, "Save").clicked();
            cancel |= ui::outline_small(ui, "Cancel", None).clicked();
        });
        ui.label(ui::muted(
            "Renames them in the whole transcript and its files. Leave empty for the \
             name found in the call.",
        ));
        if cancel {
            *edit = None;
        } else if save && let Some(e) = edit.take() {
            return Some((e.key, e.text));
        }
        return None;
    }
    let mut start = None;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = ui::S2;
        caption(ui);
        for (i, p) in t.people.iter().enumerate() {
            if i > 0 {
                ui.label(RichText::new("·").size(13.5).color(ui::INK_MUTED));
            }
            let name = egui::Label::new(
                RichText::new(&p.label)
                    .size(13.5)
                    .color(ui::INK_SECONDARY),
            )
            .selectable(false);
            if editable {
                let r = ui
                    .add(name.sense(egui::Sense::click()))
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text("Rename");
                if r.hovered() {
                    // Underline the name under the pointer.
                    let y = r.rect.bottom() - 1.0;
                    ui.painter().line_segment(
                        [egui::pos2(r.rect.left(), y), egui::pos2(r.rect.right(), y)],
                        egui::Stroke::new(1.0_f32, ui::INK_SECONDARY),
                    );
                }
                if r.clicked() {
                    start = Some(p);
                }
            } else {
                ui.add(name);
            }
        }
    });
    if let Some(p) = start {
        *edit = Some(PersonEdit::of(p));
    }
    None
}

impl PersonEdit {
    /// The field for `p`, with their name in it to correct (empty for
    /// "Participant 2").
    pub fn of(p: &Person) -> Self {
        let automatic = p.label.starts_with("Participant ")
            || p.label == Speaker::Me.label()
            || p.label.starts_with(Speaker::Others.label());
        Self {
            key: p.key,
            text: if automatic {
                String::new()
            } else {
                p.label.clone()
            },
            was: p.label.clone(),
            focus: true,
        }
    }
}

/// The dialogue as plain text, as in transcript.txt (for Copy all).
pub fn plain_text(t: &Transcript) -> String {
    t.lines
        .iter()
        .map(|l| {
            format!(
                "[{}] {}: {}\n",
                clock(l.start),
                t.people[l.who].label,
                l.text
            )
        })
        .collect()
}

/// The search field; returns how many lines match (when searching).
pub fn search(ui: &mut egui::Ui, query: &mut String, t: &Transcript) -> Option<usize> {
    let q = lower_chars(query.trim());
    let count = (!q.is_empty()).then(|| {
        t.lines
            .iter()
            .filter(|l| !matches(l, &q).is_empty())
            .count()
    });
    ui.horizontal(|ui| {
        let edit = egui::TextEdit::singleline(query)
            .hint_text("Find in transcript")
            .desired_width((ui.available_width() - 120.0).max(160.0))
            .margin(vec2(10.0, 7.0));
        ui.add(edit);
        if let Some(n) = count {
            let text = match n {
                0 => "No matches".to_string(),
                1 => "1 line".to_string(),
                n => format!("{n} lines"),
            };
            ui.label(ui::muted(text));
            if ui::close_button(ui, "Clear").clicked() {
                query.clear();
            }
        }
    });
    count
}

/// The dialogue (only the matching lines while searching).
pub fn body(ui: &mut egui::Ui, t: &Transcript, query: &str) {
    let q = lower_chars(query.trim());
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .id_salt("transcript-body")
        .show(ui, |ui| {
            ui.add_space(ui::S2);
            let width = ui.available_width() - TIME_W - ui::S3;
            let mut shown = 0;
            for line in &t.lines {
                let found = matches(line, &q);
                if !q.is_empty() && found.is_empty() {
                    continue;
                }
                shown += 1;
                let person = &t.people[line.who];
                ui.horizontal_top(|ui| {
                    ui.allocate_ui_with_layout(
                        vec2(TIME_W, 20.0),
                        Layout::left_to_right(Align::Min),
                        |ui| {
                            ui.label(
                                RichText::new(clock(line.start))
                                    .font(FontId::monospace(12.0))
                                    .color(ui::INK_MUTED),
                            );
                        },
                    );
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.label(
                            RichText::new(&person.label)
                                .font(FontId::new(13.5, ui::medium()))
                                .color(person.color),
                        );
                        ui.add(egui::Label::new(text_job(line, &found, width)).selectable(true));
                    });
                });
                ui.add_space(ui::S3);
            }
            if shown == 0 {
                ui.add_space(ui::S4);
                ui.label(ui::muted(if q.is_empty() {
                    "No speech was found in this recording."
                } else {
                    "Nothing found. Search looks at the words, in any case."
                }));
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str) -> Line {
        Line {
            start: 0.0,
            who: 0,
            lower: lower_chars(text),
            text: text.into(),
        }
    }

    #[test]
    fn search_ignores_case_in_any_alphabet() {
        let l = line("Дякую, Каті. Каті, розкажи про CASE study.");
        let q = lower_chars("каті");
        assert_eq!(matches(&l, &q), vec![(7, 11), (13, 17)]);
        assert_eq!(matches(&l, &lower_chars("case")).len(), 1);
        assert!(matches(&l, &lower_chars("олена")).is_empty());
        assert!(matches(&l, &[]).is_empty());
        // Highlighting splits the text at character, not byte, positions.
        let job = text_job(&l, &matches(&l, &q), 300.0);
        let text: String = job.text.clone();
        assert_eq!(text, l.text);
        assert_eq!(job.sections.len(), 5);
    }
}
