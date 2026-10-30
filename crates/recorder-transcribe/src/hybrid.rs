//! The Best quality: Whisper's words, completed by Parakeet.
//!
//! Measured on the user's calls (whisper.cpp large-v3-turbo on Metal against
//! Parakeet v3): Whisper gets more words right (terms like "Dashboard",
//! "Settings" in Latin, punctuation), but on call audio it silently skips
//! whole phrases, cannot write English when told the language is Ukrainian,
//! and sometimes invents text where it understands nothing ("дякую за
//! перегляд"). Parakeet misses words less often but writes more of them
//! wrongly. So:
//!
//! 1. **English regions** (Parakeet sentences that are mostly Latin, joined
//!    when less than [`EN_JOIN_S`] apart) are taken from Parakeet.
//! 2. Elsewhere the two word sequences are **aligned by text** (Whisper's
//!    timestamps around a skip are stretched over it, so time alone misses
//!    it). A run of at least [`MIN_RUN`] Parakeet words with no Whisper
//!    counterpart is inserted where it belongs.
//! 3. A run of at least [`HALLUCINATION_MIN`] Whisper words with no
//!    Parakeet counterpart and no Parakeet word at that time is dropped.
//!    Shorter runs stay: they are often real words Parakeet missed.
//!
//! Whisper runs on [`Region`]s: speech with the pauses inside kept (it is
//! more complete with natural pauses), cut where the pause is long.

use crate::SAMPLE_RATE;
use crate::transcript::Word;

/// Unmatched Parakeet words in a row needed to insert them.
pub const MIN_RUN: usize = 4;
/// Unmatched Whisper words in a row (with no Parakeet word at that time)
/// treated as invented.
pub const HALLUCINATION_MIN: usize = 5;
/// A Parakeet sentence with at least this share of Latin words is English.
pub const LATIN_SHARE: f64 = 0.7;
/// English sentences closer than this form one region.
pub const EN_JOIN_S: f64 = 3.0;

pub const SOURCE_WHISPER: &str = "whisper";
pub const SOURCE_PARAKEET: &str = "parakeet";
pub const SOURCE_PARAKEET_EN: &str = "parakeet-en";

// ── Regions for Whisper ─────────────────────────────────────────────────

/// Speech segments closer than this go into one region with the pause.
/// Whisper encodes 30-second windows whatever the length, so many short
/// regions cost far more than a few long ones; silence inside is cheap and
/// text Whisper invents there is dropped by [`merge`].
const REGION_JOIN_S: f64 = 10.0;
/// A region is cut at a segment boundary before it gets longer than this.
const REGION_MAX_S: f64 = 90.0;
/// Audio added before and after the speech.
const REGION_PAD_S: f64 = 0.2;

/// A stretch of the recording timeline, in samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub start: u64,
    pub end: u64,
}

impl Region {
    pub fn len(&self) -> usize {
        (self.end - self.start) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }

    pub fn start_s(&self) -> f64 {
        self.start as f64 / SAMPLE_RATE as f64
    }

    pub fn end_s(&self) -> f64 {
        self.end as f64 / SAMPLE_RATE as f64
    }
}

/// Joins voice-activity segments `(start, len)` (samples, in order) into
/// regions; `total` is the length of the recording in samples.
pub fn regions(segments: &[(u64, usize)], total: u64) -> Vec<Region> {
    let sr = SAMPLE_RATE as f64;
    let pad = (REGION_PAD_S * sr) as u64;
    let join = (REGION_JOIN_S * sr) as u64;
    let max = (REGION_MAX_S * sr) as u64;
    let mut out: Vec<Region> = Vec::new();
    for &(start, len) in segments {
        let (a, b) = (
            start.saturating_sub(pad),
            (start + len as u64 + pad).min(total),
        );
        match out.last_mut() {
            Some(r) if a <= r.end + join && b - r.start <= max => r.end = r.end.max(b),
            _ => out.push(Region { start: a, end: b }),
        }
    }
    out
}

// ── Merging ─────────────────────────────────────────────────────────────

/// Lower case, letters, digits and apostrophes only.
fn norm(t: &str) -> String {
    t.chars()
        .filter(|c| c.is_alphanumeric() || *c == '\'' || *c == '’')
        .flat_map(char::to_lowercase)
        .collect()
}

/// Character similarity 0..=1: 2·LCS / (|a| + |b|).
fn similarity(a: &str, b: &str) -> f64 {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let mut prev = vec![0usize; b.len() + 1];
    for x in &a {
        let mut cur = vec![0usize; b.len() + 1];
        for (j, y) in b.iter().enumerate() {
            cur[j + 1] = if x == y {
                prev[j] + 1
            } else {
                prev[j + 1].max(cur[j])
            };
        }
        prev = cur;
    }
    2.0 * prev[b.len()] as f64 / (a.len() + b.len()) as f64
}

/// Global alignment of two word lists: pairs (Parakeet index, Whisper index),
/// either side `None` for a word without a counterpart. Similar words
/// ("потера" / "Тарас") pair up; unrelated ones would rather stay apart.
fn align(p: &[Word], w: &[Word]) -> Vec<(Option<usize>, Option<usize>)> {
    const GAP: f64 = -0.4;
    let pn: Vec<String> = p.iter().map(|x| norm(&x.text)).collect();
    let wn: Vec<String> = w.iter().map(|x| norm(&x.text)).collect();
    let pair = |i: usize, j: usize| {
        let s = if pn[i] == wn[j] {
            1.0
        } else {
            similarity(&pn[i], &wn[j])
        };
        s - 0.5
    };
    let (n, m) = (p.len(), w.len());
    let mut score = vec![vec![0.0f64; m + 1]; n + 1];
    for (i, row) in score.iter_mut().enumerate() {
        row[0] = i as f64 * GAP;
    }
    for (j, cell) in score[0].iter_mut().enumerate() {
        *cell = j as f64 * GAP;
    }
    for i in 1..=n {
        for j in 1..=m {
            score[i][j] = (score[i - 1][j - 1] + pair(i - 1, j - 1))
                .max(score[i - 1][j] + GAP)
                .max(score[i][j - 1] + GAP);
        }
    }
    let (mut i, mut j) = (n, m);
    let mut out = Vec::with_capacity(n + m);
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && (score[i][j] - (score[i - 1][j - 1] + pair(i - 1, j - 1))).abs() < 1e-9
        {
            out.push((Some(i - 1), Some(j - 1)));
            i -= 1;
            j -= 1;
        } else if i > 0 && (score[i][j] - (score[i - 1][j] + GAP)).abs() < 1e-9 {
            out.push((Some(i - 1), None));
            i -= 1;
        } else {
            out.push((None, Some(j - 1)));
            j -= 1;
        }
    }
    out.reverse();
    out
}

fn is_latin(w: &Word) -> bool {
    w.text.chars().any(|c| c.is_ascii_alphabetic())
}

fn sentences(words: &[Word]) -> Vec<&[Word]> {
    let mut out = Vec::new();
    let mut start = 0;
    for i in 0..words.len() {
        let pause = i > start && words[i].start - words[i - 1].end > 1.5;
        if pause {
            out.push(&words[start..i]);
            start = i;
        }
        if words[i].text.ends_with(['.', '?', '!', '…']) {
            out.push(&words[start..=i]);
            start = i + 1;
        }
    }
    if start < words.len() {
        out.push(&words[start..]);
    }
    out
}

/// Time ranges (seconds) of English speech according to Parakeet.
fn english_regions(parakeet: &[Word]) -> Vec<(f64, f64)> {
    let mut out: Vec<(f64, f64)> = Vec::new();
    for s in sentences(parakeet) {
        let latin = s.iter().filter(|w| is_latin(w)).count();
        if s.len() >= 3 && latin as f64 / s.len() as f64 >= LATIN_SHARE {
            let (a, b) = (s[0].start, s[s.len() - 1].end);
            match out.last_mut() {
                Some(r) if a - r.1 <= EN_JOIN_S => r.1 = b,
                _ => out.push((a, b)),
            }
        }
    }
    out.into_iter().map(|(a, b)| (a - 0.5, b + 0.5)).collect()
}

fn tagged(w: &Word, source: &str) -> Word {
    Word {
        source: Some(source.to_string()),
        ..w.clone()
    }
}

/// Merges one region's (or one channel's) words, both on the recording
/// timeline. The result is in reading order.
pub fn merge(parakeet: &[Word], whisper: &[Word]) -> Vec<Word> {
    let regions = english_regions(parakeet);
    let inside = |w: &Word| {
        let mid = (w.start + w.end) / 2.0;
        regions.iter().any(|&(a, b)| a <= mid && mid <= b)
    };
    let p: Vec<Word> = parakeet.iter().filter(|w| !inside(w)).cloned().collect();
    let w: Vec<Word> = whisper.iter().filter(|w| !inside(w)).cloned().collect();

    // Whisper words in order; skipped Parakeet runs where they belong.
    let mut seq: Vec<(Word, bool)> = Vec::new(); // (word, Whisper word without counterpart)
    let mut run: Vec<Word> = Vec::new();
    let flush = |run: &mut Vec<Word>, seq: &mut Vec<(Word, bool)>, next: Option<&Word>| {
        let mut r: &[Word] = run;
        if let (Some(first), Some((last, _))) = (r.first(), seq.last())
            && norm(&first.text) == norm(&last.text)
        {
            r = &r[1..];
        }
        if let (Some(end), Some(next)) = (r.last(), next)
            && norm(&end.text) == norm(&next.text)
        {
            r = &r[..r.len() - 1];
        }
        if r.len() >= MIN_RUN {
            seq.extend(r.iter().map(|x| (tagged(x, SOURCE_PARAKEET), false)));
        }
        run.clear();
    };
    for (i, j) in align(&p, &w) {
        match (i, j) {
            (_, Some(j)) => {
                flush(&mut run, &mut seq, Some(&w[j]));
                seq.push((tagged(&w[j], SOURCE_WHISPER), i.is_none()));
            }
            (Some(i), None) => run.push(p[i].clone()),
            (None, None) => {}
        }
    }
    flush(&mut run, &mut seq, None);

    // Invented text: long unmatched Whisper runs with no Parakeet word near.
    let mut k = 0;
    while k < seq.len() {
        if !seq[k].1 {
            k += 1;
            continue;
        }
        let mut e = k;
        while e < seq.len() && seq[e].1 {
            e += 1;
        }
        let (a, b) = (seq[k].0.start - 0.3, seq[e - 1].0.end + 0.3);
        if e - k >= HALLUCINATION_MIN && !parakeet.iter().any(|x| a <= x.start && x.start <= b) {
            seq.drain(k..e);
        } else {
            k = e;
        }
    }
    let mut out: Vec<Word> = seq.into_iter().map(|(w, _)| w).collect();

    // English regions go in by time (Whisper's words there were removed).
    for &(a, b) in &regions {
        let block: Vec<Word> = parakeet
            .iter()
            .filter(|w| {
                let mid = (w.start + w.end) / 2.0;
                a <= mid && mid <= b
            })
            .map(|w| tagged(w, SOURCE_PARAKEET_EN))
            .collect();
        let at = out.iter().position(|w| w.start > a).unwrap_or(out.len());
        out.splice(at..at, block);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Words from a sentence, 0.3 s apart from `t`.
    fn words(text: &str, t: f64) -> Vec<Word> {
        text.split_whitespace()
            .enumerate()
            .map(|(i, x)| Word::new(x, t + i as f64 * 0.3, t + i as f64 * 0.3 + 0.25))
            .collect()
    }

    fn text(ws: &[Word]) -> String {
        ws.iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn regions_join_short_pauses_and_cut_long_speech() {
        let s = |sec: f64| (sec * SAMPLE_RATE as f64) as u64;
        let segs = [
            (s(1.0), s(2.0) as usize),  // 1-3 s
            (s(4.0), s(1.0) as usize),  // 1 s pause: joined
            (s(16.0), s(1.0) as usize), // 11 s pause: new region
        ];
        let r = regions(&segs, s(100.0));
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].start, s(0.8));
        assert_eq!(r[0].end, s(5.2));
        assert_eq!(r[1].start, s(15.8));
        // 50 one-second segments every 2 s: cut before 90 s.
        let long: Vec<_> = (0..50)
            .map(|i| (s(i as f64 * 2.0 + 1.0), s(1.0) as usize))
            .collect();
        let r = regions(&long, s(200.0));
        assert!(r.len() == 2 && r.iter().all(|x| x.len() as f64 <= REGION_MAX_S * 16_000.0));
        // Padding stays inside the recording.
        assert_eq!(regions(&[(0, 100)], 200)[0], Region { start: 0, end: 200 });
    }

    /// From fragment 01: Whisper skipped a sentence and stretched the
    /// timestamps of its neighbours over it.
    #[test]
    fn a_skipped_sentence_is_filled_in_reading_order() {
        let parakeet = words(
            "Ну, насправді я мені просто було смішно, потера сказав за вік, але окей. Насправді в мене якийсь стан",
            0.4,
        );
        let mut whisper = words("Ну, насправді, в мене якийсь стан", 0.0);
        // Stretched: "насправді" 0.6-3.1 s, "в мене" from 3.1 s.
        whisper[1].end = 3.1;
        for (k, w) in whisper.iter_mut().enumerate().skip(2) {
            w.start = 3.1 + k as f64;
            w.end = w.start + 0.5;
        }
        let m = merge(&parakeet, &whisper);
        let t = text(&m);
        assert!(
            t.contains("просто було смішно, потера сказав за вік, але окей."),
            "{t}"
        );
        // Whisper's "насправді," pairs with Parakeet's second one.
        assert!(t.starts_with("Ну, насправді я мені просто"), "{t}");
        assert!(t.ends_with("в мене якийсь стан"), "{t}");
        assert_eq!(
            t.matches("насправді").count() + t.matches("Насправді").count(),
            2,
            "{t}"
        );
        assert!(
            m.iter()
                .any(|w| w.source.as_deref() == Some(SOURCE_PARAKEET))
        );
    }

    #[test]
    fn short_differences_and_misrecognitions_keep_whisper() {
        // Parakeet's own wording of the same words: no insertion.
        let parakeet = words("в нас є багатьом з я що робити але просто кажуть", 0.0);
        let whisper = words("в нас є баги в нас є що робити але просто кажу", 0.0);
        let m = merge(&parakeet, &whisper);
        assert_eq!(text(&m), text(&whisper));
        assert!(
            m.iter()
                .all(|w| w.source.as_deref() == Some(SOURCE_WHISPER))
        );
    }

    #[test]
    fn english_comes_from_parakeet_and_invented_text_goes() {
        let mut parakeet = words("This is like data from some other kind of sports.", 0.0);
        parakeet.extend(words("Especially for them because it matters.", 12.0));
        let mut whisper = words("Дякую за перегляд і це добре", 0.0);
        // Invented between the English sentences, where Parakeet heard nothing.
        whisper.extend(words("не може бути в іншому або іншому що це не може", 6.0));
        let m = merge(&parakeet, &whisper);
        let t = text(&m);
        assert!(t.starts_with("This is like data"), "{t}");
        assert!(t.contains("Especially for them"), "{t}");
        assert!(!t.contains("перегляд") && !t.contains("не може"), "{t}");
        assert!(
            m.iter()
                .all(|w| w.source.as_deref() == Some(SOURCE_PARAKEET_EN))
        );
    }

    /// From fragment 08: "Я просто, ну," is real speech Parakeet missed.
    #[test]
    fn short_whisper_only_words_stay() {
        let mut parakeet = words("Ну я їм за це писала.", 56.0);
        parakeet.extend(words("Ти розумієте, пінгати їх за домен", 58.8));
        let mut whisper = words("Ну, я їм за це писала.", 56.0);
        whisper.extend(words("Я просто, ну,", 57.4));
        whisper.extend(words("ти розумієте, пінгати їх за домен", 58.4));
        let t = text(&merge(&parakeet, &whisper));
        assert!(t.contains("писала. Я просто, ну, ти розумієте"), "{t}");
    }

    #[test]
    fn similarity_and_normalization() {
        assert!(similarity(&norm("Тарас"), &norm("потера")) > 0.3);
        assert_eq!(norm("Settings,"), "settings");
        assert_eq!(similarity("", "x"), 0.0);
    }
}
