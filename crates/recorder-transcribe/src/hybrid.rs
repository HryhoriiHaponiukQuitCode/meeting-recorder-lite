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
//! 1. **English** is taken from Parakeet: a whole region when most of its
//!    Parakeet words are Latin ([`REGION_LATIN_SHARE`]; an English call),
//!    else the Parakeet sentences that are mostly Latin, joined when less
//!    than [`EN_JOIN_S`] apart.
//! 2. Elsewhere the two word sequences are **aligned by text** (Whisper's
//!    timestamps around a skip are stretched over it, so time alone misses
//!    it). A run of at least [`MIN_RUN`] Parakeet words with no Whisper
//!    counterpart is inserted where it belongs.
//! 3. Whisper words with no Parakeet counterpart are dropped as invented
//!    when the run is at least [`HALLUCINATION_MIN`] long with no Parakeet
//!    word at that time, when Parakeet heard nothing within [`ISOLATED_S`]
//!    of it (a lone "Дякую." on a quiet microphone), or when it is one of
//!    Whisper's known inventions ("Субтитрувальниця…", "Дякую за
//!    перегляд!"). Other short runs stay: they are often real words
//!    Parakeet missed.
//!
//! 4. Whisper sometimes falls into writing without punctuation or capitals
//!    for minutes (after long pauses inside a region). Its words stay, but
//!    a run of at least [`UNPUNCTUATED_RUN`] of them without punctuation
//!    takes the punctuation of the Parakeet words it aligns with, and
//!    capitals after the sentence ends ([`borrow_punctuation`]). Measured
//!    on two real calls: cutting regions shorter or giving Whisper a
//!    punctuated prompt fixed the punctuation too, but made the words worse
//!    (blind judgement, 46 and 40 likelier spots against 69 and 60).
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
/// Shorter unmatched Whisper runs are dropped when no Parakeet word is
/// within this many seconds.
pub const ISOLATED_S: f64 = 1.0;
/// Text Whisper invents on silence, noise and music (subtitle credits from
/// its training data), lower case without punctuation.
const KNOWN_INVENTIONS: &[&str] = &[
    "субтитр",
    "дякую за перегляд",
    "дякуємо за перегляд",
    "продовження слідує",
    "підписуйтесь",
    "підписуйтеся",
    "редактор субтитрів",
    "amara",
    "thanks for watching",
    "thank you for watching",
];
/// Paired words less similar than this (0..=1) are unrelated.
const UNRELATED_BELOW: f64 = 0.25;
/// A Parakeet sentence with at least this share of Latin words is English.
pub const LATIN_SHARE: f64 = 0.7;
/// A region with at least this share of Latin Parakeet words is English as
/// a whole: Whisper told to write Ukrainian turns English into Cyrillic.
pub const REGION_LATIN_SHARE: f64 = 0.5;
/// English sentences closer than this form one region.
pub const EN_JOIN_S: f64 = 3.0;
/// Latin text is English only with at least this share of English
/// function words: Parakeet sometimes writes Ukrainian as Polish or
/// Croatian ("To, якщо możesz, to poszerwit lask", "Dobre, co to gdzie
/// to?"), which must not replace Whisper's Ukrainian.
pub const ENGLISH_WORDS_SHARE: f64 = 0.1;
/// Latin text with at least this share of words in letters English does
/// not have ("ż", "č") is not English (a name like "Asunción" is fine).
const FOREIGN_LETTERS_SHARE: f64 = 0.2;
/// Polish, Czech and Croatian words Parakeet writes for Ukrainian ones;
/// Latin text without English function words but with one of these is
/// not English ("Dobre, co to gdzie to?"), without either it is ("Buenos
/// Aires").
const SLAVIC_WORDS: &[&str] = &[
    "co", "gdzie", "jest", "nie", "tak", "się", "że", "jak", "ale", "bo", "ten", "ta", "mam",
    "może", "dobre", "dobrze", "dobro", "kak", "što", "je", "sam", "da", "ne", "ni", "ono",
    "ona", "oni", "mi", "ti", "vi", "smo", "ste", "su", "bude", "tu", "tam", "teraz", "już",
    "jeszcze", "bardzo", "proszę", "dziękuję", "ja", "ty", "wy", "czy", "kiedy",
];
/// Common English words that are not also Polish, Czech or Croatian ones
/// ("to", "a", "i", "no", "co", "do", "na" are).
const ENGLISH_WORDS: &[&str] = &[
    "the", "and", "of", "you", "is", "are", "was", "were", "it", "it's", "that", "this", "we",
    "they", "he", "she", "in", "for", "with", "be", "have", "has", "not", "but", "what", "so",
    "if", "can", "will", "just", "my", "your", "our", "there", "here", "yes", "okay", "yeah",
    "right", "about", "at", "from", "or", "an", "me", "know", "think", "like", "get", "would",
    "should", "could", "i'm", "don't", "let's", "thank", "thanks", "please", "how", "why",
    "when", "where", "which", "who", "all", "one", "some", "them", "his", "her", "been",
    "does", "did", "go", "going", "want", "need", "see", "make", "good", "great", "sure",
    "also", "then", "than", "because", "maybe", "now", "well", "really", "very",
];
/// Whisper words in a row without any punctuation that take Parakeet's.
pub const UNPUNCTUATED_RUN: usize = 12;
/// Aligned words at least this similar (0..=1) share punctuation.
const PUNCTUATION_SIMILAR: f64 = 0.5;

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

/// Latin words that read as English: few words with letters English does
/// not have ([`FOREIGN_LETTERS_SHARE`]), and enough of its function words
/// ([`ENGLISH_WORDS_SHARE`]) or none of [`SLAVIC_WORDS`].
fn looks_english(words: &[Word]) -> bool {
    let latin: Vec<&Word> = words.iter().filter(|w| is_latin(w)).collect();
    if latin.is_empty() {
        return false;
    }
    let foreign = latin
        .iter()
        .filter(|w| {
            w.text.chars().any(|c| {
                c.is_alphabetic() && !c.is_ascii() && !('\u{0400}'..='\u{04ff}').contains(&c)
            })
        })
        .count();
    let english = latin
        .iter()
        .filter(|w| ENGLISH_WORDS.contains(&norm(&w.text).replace('’', "'").as_str()))
        .count();
    if foreign as f64 >= FOREIGN_LETTERS_SHARE * latin.len() as f64 {
        return false;
    }
    english as f64 >= ENGLISH_WORDS_SHARE * latin.len() as f64
        || !latin
            .iter()
            .any(|w| SLAVIC_WORDS.contains(&norm(&w.text).as_str()))
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
        if s.len() >= 3 && latin as f64 / s.len() as f64 >= LATIN_SHARE && looks_english(s) {
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
    let latin = parakeet.iter().filter(|w| is_latin(w)).count();
    if !parakeet.is_empty()
        && latin as f64 >= REGION_LATIN_SHARE * parakeet.len() as f64
        && looks_english(parakeet)
    {
        return parakeet
            .iter()
            .map(|w| tagged(w, SOURCE_PARAKEET_EN))
            .collect();
    }
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
    // A pair of unrelated words (another alphabet, or hardly a letter in
    // common) does not confirm Whisper's word.
    let unrelated = |i: usize, j: usize| {
        is_latin(&p[i]) != is_latin(&w[j])
            || similarity(&norm(&p[i].text), &norm(&w[j].text)) < UNRELATED_BELOW
    };
    for (i, j) in align(&p, &w) {
        match (i, j) {
            (_, Some(j)) => {
                flush(&mut run, &mut seq, Some(&w[j]));
                let alone = i.is_none_or(|i| unrelated(i, j));
                let mut word = tagged(&w[j], SOURCE_WHISPER);
                // Parakeet's times are the reliable ones (Whisper stretches
                // words over pauses, up to seconds).
                if let (Some(i), false) = (i, alone) {
                    word.start = p[i].start;
                    word.end = p[i].end;
                }
                seq.push((word, alone));
            }
            (Some(i), None) => run.push(p[i].clone()),
            (None, None) => {}
        }
    }
    flush(&mut run, &mut seq, None);

    // Known inventions, unless Parakeet has the phrase too.
    let heard_text = parakeet
        .iter()
        .map(|x| norm(&x.text))
        .collect::<Vec<_>>()
        .join(" ");
    let invention_in = |i: usize, len: usize| {
        let phrase = seq[i..i + len]
            .iter()
            .map(|(w, _)| norm(&w.text))
            .collect::<Vec<_>>()
            .join(" ");
        KNOWN_INVENTIONS
            .iter()
            .any(|p| phrase.contains(p) && !heard_text.contains(p))
    };
    let mut known = vec![false; seq.len()];
    for i in 0..seq.len() {
        for len in 1..=4.min(seq.len() - i) {
            // Only the smallest stretch holding the phrase.
            let smallest = len == 1 || !(invention_in(i + 1, len - 1) || invention_in(i, len - 1));
            if smallest && invention_in(i, len) {
                known[i..i + len].iter_mut().for_each(|k| *k = true);
            }
        }
    }
    // The rest of an unconfirmed run with an invention in it goes too
    // ("Субтитрувальниця Оля Шор").
    for i in 0..seq.len() {
        if known[i] && seq[i].1 {
            let mut j = i;
            while j > 0 && seq[j - 1].1 {
                j -= 1;
                known[j] = true;
            }
            let mut j = i;
            while j + 1 < seq.len() && seq[j + 1].1 {
                j += 1;
                known[j] = true;
            }
        }
    }
    let mut keep = known.iter().map(|k| !k);
    seq.retain(|_| keep.next().unwrap_or(true));

    // Invented text (see the module notes).
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
        let (start, end) = (seq[k].0.start, seq[e - 1].0.end);
        let heard = |margin: f64| {
            parakeet
                .iter()
                .any(|x| start - margin <= x.end && x.start <= end + margin)
        };
        let invented = (e - k >= HALLUCINATION_MIN && !heard(0.3)) || !heard(ISOLATED_S);
        if invented {
            seq.drain(k..e);
        } else {
            k = e;
        }
    }
    let alone: Vec<bool> = seq.iter().map(|(_, a)| *a).collect();
    let mut out: Vec<Word> = seq.into_iter().map(|(w, _)| w).collect();
    fit_times(&mut out, &alone);

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
    borrow_punctuation(&mut out, parakeet);
    out
}

/// Whisper words without a Parakeet counterpart (`alone`) keep their own
/// times only where those fit between their neighbours and are not
/// stretched; otherwise a run of them shares the gap between the
/// neighbours evenly.
fn fit_times(words: &mut [Word], alone: &[bool]) {
    const LONGEST_S: f64 = 1.5;
    let mut k = 0;
    while k < words.len() {
        if !alone[k] {
            k += 1;
            continue;
        }
        let mut e = k;
        while e < words.len() && alone[e] {
            e += 1;
        }
        let lo = if k > 0 { words[k - 1].end } else { words[k].start };
        let hi = if e < words.len() {
            words[e].start
        } else {
            words[e - 1].end
        };
        let off = words[k..e]
            .iter()
            .any(|w| w.start < lo - 0.01 || w.end > hi + 0.01 || w.end - w.start > LONGEST_S);
        if off && hi > lo {
            let step = ((hi - lo) / (e - k) as f64).min(LONGEST_S);
            // Close to the side that has a neighbour (or the start).
            let base = if k == 0 && e < words.len() {
                hi - step * (e - k) as f64
            } else {
                lo
            };
            for (m, w) in words[k..e].iter_mut().enumerate() {
                w.start = base + step * m as f64;
                w.end = w.start + step;
            }
        }
        k = e;
    }
}

fn has_punctuation(t: &str) -> bool {
    t.ends_with(['.', ',', '?', '!', '…', ':', ';'])
}

fn ends_sentence(t: &str) -> bool {
    t.ends_with(['.', '?', '!', '…'])
}

/// Long runs of Whisper words without punctuation take it from the Parakeet
/// words they align with; a word after a sentence end gets a capital. The
/// words themselves do not change.
fn borrow_punctuation(words: &mut [Word], parakeet: &[Word]) {
    let whisper = |w: &Word| w.source.as_deref() == Some(SOURCE_WHISPER);
    let mut i = 0;
    while i < words.len() {
        let mut e = i;
        while e < words.len() && whisper(&words[e]) && !has_punctuation(&words[e].text) {
            e += 1;
        }
        if e - i < UNPUNCTUATED_RUN {
            i = e.max(i + 1);
            continue;
        }
        let (a, b) = (words[i].start - 1.0, words[e - 1].end + 1.0);
        let p: Vec<Word> = parakeet
            .iter()
            .filter(|x| a <= x.start && x.start <= b)
            .cloned()
            .collect();
        let run: Vec<Word> = words[i..e].to_vec();
        // Whether Parakeet starts the sentence with this word (a capital).
        let mut capital = vec![false; run.len()];
        for (pi, wj) in align(&p, &run) {
            let (Some(pi), Some(wj)) = (pi, wj) else {
                continue;
            };
            if similarity(&norm(&p[pi].text), &norm(&run[wj].text)) < PUNCTUATION_SIMILAR {
                continue;
            }
            let tail: String = p[pi]
                .text
                .chars()
                .rev()
                .take_while(|c| ".,?!…:;".contains(*c))
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            words[i + wj].text.push_str(&tail);
            capital[wj] = p[pi].text.chars().next().is_some_and(char::is_uppercase);
        }
        for k in i..e {
            let after_end = if k == 0 {
                capital[0]
            } else {
                ends_sentence(&words[k - 1].text)
            };
            if after_end && let Some(first) = words[k].text.chars().next()
                && first.is_lowercase()
            {
                let rest = &words[k].text[first.len_utf8()..];
                words[k].text = first.to_uppercase().collect::<String>() + rest;
            }
        }
        i = e;
    }
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

    #[test]
    fn a_long_run_without_punctuation_takes_parakeets() {
        let w = words(
            "ну там буде видно цю транзакцію чи ні а це комісія за переказ чи якась інша",
            0.0,
        );
        let p = words(
            "Ну, там буде видно цю транзакцію чи ні? А це комісія за переказ, чи якась інша?",
            0.0,
        );
        let out = merge(&p, &w);
        assert_eq!(
            text(&out),
            "Ну, там буде видно цю транзакцію чи ні? А це комісія за переказ, чи якась інша?"
        );
        assert!(out.iter().all(|x| x.source.as_deref() == Some(SOURCE_WHISPER)));
        // Whisper's own words stay even where Parakeet differs, and a short
        // run is left as it is.
        let w = words("так добре давай далі", 0.0);
        let p = words("Так. Добре. Давай далі.", 0.0);
        assert_eq!(text(&merge(&p, &w)), "так добре давай далі");
    }

    #[test]
    fn whisper_words_take_parakeets_times() {
        // Whisper stretches "ну" over a pause and "кажу" too.
        let mut w = words("ну я тобі кажу що завтра", 0.0);
        w[0].end = 4.0;
        w[3].start = 0.5;
        w[3].end = 9.0;
        let p = words("Ну, я тобі кажу, що завтра.", 3.0);
        let out = merge(&p, &w);
        assert_eq!(out.len(), 6);
        for (o, q) in out.iter().zip(&p) {
            assert_eq!((o.start, o.end), (q.start, q.end), "{}", o.text);
        }
        // A word only Whisper heard, stretched: fitted between neighbours.
        let mut w = words("ну я тобі дуже кажу що завтра", 0.0);
        w[3].start = 0.0;
        w[3].end = 20.0;
        let p = words("Ну я тобі кажу що завтра", 0.0);
        let out = merge(&p, &w);
        let d = out.iter().find(|x| x.text == "дуже").unwrap();
        assert!(d.start >= out[2].end - 1e-9 && d.end <= out[4].start + 1e-9, "{d:?}");
    }

    #[test]
    fn polish_looking_latin_is_not_english() {
        for latin in [
            "To, якщо możesz, to poszerwit lask. To trochę zroczniejszy bude.",
            "Dobre, co to gdzie to?",
            "kažem predlazimo trideset dolar",
        ] {
            let p = words(latin, 0.0);
            let w = words("Так, якщо можна поширити, будь ласка", 0.0);
            let out = merge(&p, &w);
            assert!(
                out.iter().all(|x| x.source.as_deref() != Some(SOURCE_PARAKEET_EN)),
                "{latin}: {}",
                text(&out)
            );
        }
        let en = words("Okay, thank you, I think that is fine for now.", 0.0);
        assert!(looks_english(&en));
        let names = words("Since the foundation of Asunción in 1537, Paraguay has managed", 0.0);
        assert!(looks_english(&names));
        assert!(looks_english(&words("Dashboard settings profile", 0.0)));
        assert!(looks_english(&words("Buenos Aires.", 0.0)));
        assert!(!looks_english(&words("Dobre, co to gdzie to?", 0.0)));
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

    /// From real calls: Whisper's lone phrases on a quiet microphone and
    /// its subtitle credits go; Cyrillic for English speech never appears.
    #[test]
    fn lone_and_known_inventions_go() {
        let parakeet = words("Добре, тоді давай так і зробимо.", 10.0);
        let mut whisper = words("Дякую.", 2.0); // nothing heard near
        whisper.extend(words("Добре, тоді давай так і зробимо.", 10.0));
        whisper.extend(words("Субтитрувальниця Оля Шор", 12.2)); // right after speech
        let t = text(&merge(&parakeet, &whisper));
        assert_eq!(t, "Добре, тоді давай так і зробимо.");

        // A short English reply: Whisper's "translation" paired with it goes.
        let m = merge(&words("Yeah.", 5.0), &words("Дякую.", 5.0));
        assert_eq!(text(&m), "Yeah.");
        // A known invention over real speech goes; the speech stays.
        let p = words("Добре, тоді до завтра", 1.9);
        let w = words("Дякую за перегляд! Добре, тоді до завтра", 1.0);
        assert_eq!(text(&merge(&p, &w)), "Добре, тоді до завтра");
        let p = words("Дякую за перегляд, побачимось завтра", 1.0);
        let w = words("Дякую за перегляд, побачимось завтра", 1.0);
        assert_eq!(text(&merge(&p, &w)), "Дякую за перегляд, побачимось завтра");

        // A region of an English call: all of it from Parakeet.
        let mut p = words("Oh got it. Right. Okay, that makes sense.", 0.0);
        p.extend(words("I don't think there is.", 4.0));
        let w = words("О, \"віпсі\" я не думаю, I don't think there is.", 0.0);
        let m = merge(&p, &w);
        assert_eq!(
            text(&m),
            "Oh got it. Right. Okay, that makes sense. I don't think there is."
        );
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
