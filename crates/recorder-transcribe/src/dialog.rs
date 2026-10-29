//! One dialogue from both channels, and echo detection.
//!
//! When the call plays through loudspeakers (and echo cancellation is off or
//! not enough), the microphone also hears the other side, so their words
//! appear twice: once on the right channel and, quieter, on the left. Such a
//! "Me" utterance is marked as echo when it overlaps "Others" speech in time
//! and most of its words are found, in order, in what "Others" said then.
//!
//! The threshold: re-recognizing the same speech at a lower level changes a
//! few words (typically 10-30% here), while two people really talking at
//! once share only scattered function words. Requiring 60% of the words, in
//! order (longest common subsequence), separates the two; one- and two-word
//! utterances must match completely, because "так" or "ok" overlap by chance.

use crate::Speaker;
use crate::transcript::Utterance;

/// Echo arrives slightly later than the original and recognizer timing is
/// approximate, so the time window is widened by this much on each side.
const ECHO_WINDOW_S: f64 = 1.0;
const ECHO_MIN_RATIO: f64 = 0.6;
/// A same-speaker run continues if the next utterance starts within this.
const TURN_GAP_S: f64 = 4.0;

/// Both channels in chronological order, echo marked.
pub fn merge(mut me: Vec<Utterance>, others: Vec<Utterance>) -> Vec<Utterance> {
    mark_echo(&mut me, &others);
    let mut all = me;
    all.extend(others);
    all.sort_by(|a, b| a.start.total_cmp(&b.start).then(a.end.total_cmp(&b.end)));
    all
}

pub fn mark_echo(me: &mut [Utterance], others: &[Utterance]) {
    for u in me.iter_mut().filter(|u| u.speaker == Speaker::Me) {
        let mine = normalized_words(&u.text);
        if mine.is_empty() {
            continue;
        }
        let (from, to) = (u.start - ECHO_WINDOW_S, u.end + ECHO_WINDOW_S);
        let theirs: Vec<String> = others
            .iter()
            .filter(|o| o.end >= from && o.start <= to)
            .flat_map(|o| normalized_words(&o.text))
            .collect();
        if theirs.is_empty() {
            continue;
        }
        let ratio = lcs(&mine, &theirs) as f64 / mine.len() as f64;
        u.echo = if mine.len() <= 2 {
            ratio >= 1.0
        } else {
            ratio >= ECHO_MIN_RATIO
        };
    }
}

/// Lower case, letters, digits and inner apostrophes only.
pub fn normalized_words(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '\'' || c == '’'))
        .map(|w| w.trim_matches(['\'', '’']).to_lowercase())
        .filter(|w| !w.is_empty())
        .collect()
}

/// Length of the longest common subsequence of two word lists.
fn lcs(a: &[String], b: &[String]) -> usize {
    let mut prev = vec![0usize; b.len() + 1];
    let mut cur = vec![0usize; b.len() + 1];
    for x in a {
        for (j, y) in b.iter().enumerate() {
            cur[j + 1] = if x == y {
                prev[j] + 1
            } else {
                prev[j + 1].max(cur[j])
            };
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Consecutive utterances of one speaker, for the readable text file.
#[derive(Clone, Debug, PartialEq)]
pub struct Turn {
    pub speaker: Speaker,
    pub start: f64,
    pub end: f64,
    pub text: String,
}

/// Groups the dialogue (echo left out) into turns.
pub fn turns(dialog: &[Utterance]) -> Vec<Turn> {
    let mut out: Vec<Turn> = Vec::new();
    for u in dialog.iter().filter(|u| !u.echo) {
        match out.last_mut() {
            Some(t) if t.speaker == u.speaker && u.start - t.end <= TURN_GAP_S => {
                t.text.push(' ');
                t.text.push_str(&u.text);
                t.end = t.end.max(u.end);
            }
            _ => out.push(Turn {
                speaker: u.speaker,
                start: u.start,
                end: u.end,
                text: u.text.clone(),
            }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utt(speaker: Speaker, start: f64, end: f64, text: &str) -> Utterance {
        Utterance {
            speaker,
            start,
            end,
            text: text.into(),
            words: vec![],
            echo: false,
        }
    }

    #[test]
    fn echo_of_the_other_side_is_marked() {
        let others = vec![utt(
            Speaker::Others,
            10.0,
            14.0,
            "Давайте перенесемо зустріч на четвер, бо в середу я зайнятий.",
        )];
        let me = vec![
            // The same words, slightly misrecognized and a bit late.
            utt(
                Speaker::Me,
                10.2,
                14.3,
                "давайте перенесемо зустріч на чотвер бо в середу я занятий",
            ),
            // Really talking over them.
            utt(Speaker::Me, 12.0, 13.5, "Так, мені теж підходить четвер"),
            // Later, alone.
            utt(Speaker::Me, 20.0, 22.0, "Добре, тоді до четверга."),
        ];
        let d = merge(me, others);
        let flags: Vec<(&str, bool)> = d
            .iter()
            .filter(|u| u.speaker == Speaker::Me)
            .map(|u| (u.text.as_str(), u.echo))
            .collect();
        assert!(flags[0].1, "{flags:?}");
        assert!(!flags[1].1, "{flags:?}");
        assert!(!flags[2].1, "{flags:?}");
        // Chronological.
        assert!(d.windows(2).all(|w| w[0].start <= w[1].start));
    }

    #[test]
    fn short_replies_need_an_exact_match() {
        let others = vec![utt(Speaker::Others, 5.0, 6.0, "Так, звісно.")];
        let mut me = vec![
            utt(Speaker::Me, 5.1, 5.6, "так"),
            utt(Speaker::Me, 5.2, 5.8, "Ні, не так"),
        ];
        mark_echo(&mut me, &others);
        assert!(me[0].echo);
        assert!(!me[1].echo);
    }

    #[test]
    fn turns_group_one_speaker_and_skip_echo() {
        let mut echo = utt(Speaker::Me, 2.5, 3.0, "a b c");
        echo.echo = true;
        let d = vec![
            utt(Speaker::Others, 0.0, 2.0, "Привіт."),
            utt(Speaker::Others, 2.2, 3.0, "Як ти?"),
            echo,
            utt(Speaker::Me, 3.5, 4.0, "Добре."),
            utt(Speaker::Me, 20.0, 21.0, "Ще одне."),
        ];
        let t = turns(&d);
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].text, "Привіт. Як ти?");
        assert_eq!(t[1].speaker, Speaker::Me);
        assert_eq!(t[2].text, "Ще одне.");
    }
}
