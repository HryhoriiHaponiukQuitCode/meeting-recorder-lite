//! Telling apart the voices on the system-audio channel ("Others 1",
//! "Others 2", …). Pure logic; the voice prints come from the transcriber.
//!
//! - Speech segments (the VAD of the recognition pass) are cut into 1.5 s
//!   windows every 0.75 s; each window gets a voice print (CAM++). Only
//!   full windows form the voices; shorter bits join the nearest one.
//! - Average-linkage clustering of the prints with a cosine-distance
//!   threshold of 0.62: the middle of the range (0.58–0.68) that gave the
//!   same speakers on the user's calls, Ukrainian and English. Above 0.70
//!   different people started to merge.
//! - Voices with under 3% of the windows or of the words' time are folded
//!   into the nearest one,
//!   a window takes the majority label of itself and its neighbours, and
//!   every word takes the label of the windows around it.
//!
//! Measured on five real calls (see docs/04-transcription.md): the user's
//! own voice stays one cluster, and the remote side of an English call
//! never merges with the Ukrainian ones (1.3% of words).

use crate::SAMPLE_RATE;
use crate::transcript::Word;

pub const WINDOW_S: f64 = 1.5;
pub const STEP_S: f64 = 0.75;
/// Shorter speech segments get no print of their own.
pub const MIN_SEGMENT_S: f64 = 0.5;
/// Cosine distance at which clusters stop merging.
pub const THRESHOLD: f32 = 0.62;
/// Above this many windows the clustering runs on an even sample and the
/// rest go to the nearest voice (memory and time grow with the square).
pub const MAX_CLUSTER_POINTS: usize = 3000;
/// A voice with a smaller share of the windows is folded into another.
pub const MIN_VOICE_SHARE: f64 = 0.03;
/// On the microphone (the user's own) another voice needs at least this
/// much speech (seconds of its words); less goes to the voice that says
/// the most. Time, not words: spurious voices are many short words (one
/// person on an hour of call with headphones got two more of 31 and 38
/// words in 10–11 s, 7–9% of the words), a real second person at the
/// microphone talks for minutes (262 s and more on the user's room
/// meetings). A count of words (25 before) let the short ones through.
pub const MIC_MIN_SPEECH_S: f64 = 30.0;

/// Windows `(start, length)` in samples inside the speech segments; a
/// segment shorter than a window is one window when long enough.
pub fn windows(segments: &[(u64, usize)]) -> Vec<(u64, usize)> {
    let sr = SAMPLE_RATE as f64;
    let (win, step, min) = (
        (WINDOW_S * sr) as usize,
        (STEP_S * sr) as usize,
        (MIN_SEGMENT_S * sr) as usize,
    );
    let mut out = Vec::new();
    for &(start, len) in segments {
        if len < min {
            continue;
        }
        if len <= win {
            out.push((start, len));
            continue;
        }
        let mut at = 0;
        while at + win <= len {
            out.push((start + at as u64, win));
            at += step;
        }
        // The tail, aligned to the end of the segment.
        if at < len && at + win > len {
            out.push((start + (len - win) as u64, win));
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

fn normalized(v: &[f32]) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
    v.iter().map(|x| x / n).collect()
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Flat clusters of unit vectors: average linkage on cosine distance, cut
/// at `threshold` (as scipy `fcluster(..., criterion="distance")`). Labels
/// are 0.. in order of first appearance.
pub fn cluster(points: &[Vec<f32>], threshold: f32) -> Vec<usize> {
    let n = points.len();
    if n < 2 {
        return vec![0; n];
    }
    let mut d = vec![0f32; n * n];
    for i in 0..n {
        for j in i + 1..n {
            let v = 1.0 - dot(&points[i], &points[j]);
            d[i * n + j] = v;
            d[j * n + i] = v;
        }
    }
    // Nearest-neighbour chain: the same tree as the textbook algorithm for
    // average linkage, in O(n²).
    let mut active = vec![true; n];
    let mut size = vec![1usize; n];
    let mut parent: Vec<usize> = (0..n).collect();
    fn root(p: &mut [usize], mut x: usize) -> usize {
        while p[x] != x {
            p[x] = p[p[x]];
            x = p[x];
        }
        x
    }
    let mut chain: Vec<usize> = Vec::new();
    let mut remaining = n;
    while remaining > 1 {
        if chain.is_empty() {
            chain.push(active.iter().position(|&a| a).unwrap_or(0));
        }
        let (a, b, dist) = loop {
            let a = *chain.last().unwrap_or(&0);
            let prev = chain.len().checked_sub(2).map(|i| chain[i]);
            let mut best = prev;
            let mut best_d = prev.map_or(f32::INFINITY, |p| d[a * n + p]);
            for j in 0..n {
                if j != a && active[j] && d[a * n + j] < best_d {
                    best = Some(j);
                    best_d = d[a * n + j];
                }
            }
            let Some(b) = best else {
                break (a, a, f32::INFINITY);
            };
            if Some(b) == prev {
                chain.pop();
                chain.pop();
                break (a, b, best_d);
            }
            chain.push(b);
        };
        if a == b {
            break;
        }
        // Merge a into b.
        let (sa, sb) = (size[a] as f32, size[b] as f32);
        for k in 0..n {
            if active[k] && k != a && k != b {
                let v = (sa * d[a * n + k] + sb * d[b * n + k]) / (sa + sb);
                d[b * n + k] = v;
                d[k * n + b] = v;
            }
        }
        active[a] = false;
        size[b] += size[a];
        remaining -= 1;
        if dist <= threshold {
            let (ra, rb) = (root(&mut parent, a), root(&mut parent, b));
            parent[ra] = rb;
        }
    }
    let mut ids: Vec<Option<usize>> = vec![None; n];
    let mut next = 0;
    (0..n)
        .map(|i| {
            let r = root(&mut parent, i);
            *ids[r].get_or_insert_with(|| {
                next += 1;
                next - 1
            })
        })
        .collect()
}

fn centroids(points: &[Vec<f32>], labels: &[usize], k: usize) -> Vec<Vec<f32>> {
    let dim = points.first().map_or(0, Vec::len);
    let mut sum = vec![vec![0f32; dim]; k];
    for (p, &l) in points.iter().zip(labels) {
        for (s, x) in sum[l].iter_mut().zip(p) {
            *s += x;
        }
    }
    sum.iter().map(|s| normalized(s)).collect()
}

fn nearest(p: &[f32], cents: &[Vec<f32>], allowed: &[bool]) -> usize {
    let mut best = (0, f32::NEG_INFINITY);
    for (i, c) in cents.iter().enumerate() {
        if allowed[i] {
            let s = dot(p, c);
            if s > best.1 {
                best = (i, s);
            }
        }
    }
    best.0
}

/// Tells the voices apart and sets `voice` (1-based, in order of first
/// appearance) on `words` from the prints of `windows` (sorted by start).
/// Voices with under [`MIN_VOICE_SHARE`] of the windows or of the words'
/// time are folded into the nearest other voice. With a single voice nothing is set, so
/// the transcript keeps plain "Others". Returns the number of voices (0 for
/// one).
pub fn assign(words: &mut [Word], windows: &[(u64, usize)], prints: &[Vec<f32>]) -> u32 {
    for w in words.iter_mut() {
        w.voice = None;
    }
    let n = prints.len().min(windows.len());
    if n == 0 || words.is_empty() {
        return 0;
    }
    let windows = &windows[..n];
    let points: Vec<Vec<f32>> = prints[..n].iter().map(|p| normalized(p)).collect();
    // Voices come from full windows only: prints of short bits ("um",
    // "okay") are unreliable and formed voices of their own. Those, and
    // everything beyond an even sample of a long call, go to the nearest.
    let full = (WINDOW_S * SAMPLE_RATE as f64) as usize;
    let mut basis: Vec<usize> = (0..n).filter(|&i| windows[i].1 >= full).collect();
    if basis.len() < 2 {
        basis = (0..n).collect();
    }
    if basis.len() > MAX_CLUSTER_POINTS {
        basis = (0..MAX_CLUSTER_POINTS)
            .map(|i| basis[i * basis.len() / MAX_CLUSTER_POINTS])
            .collect();
    }
    let sample: Vec<Vec<f32>> = basis.iter().map(|&i| points[i].clone()).collect();
    let sample_labels = cluster(&sample, THRESHOLD);
    let k = count(&sample_labels);
    // Voices of a few stray full windows go before the short bits are
    // shared out (fillers would otherwise flock to them).
    let mut size = vec![0usize; k];
    for &l in &sample_labels {
        size[l] += 1;
    }
    let biggest = (0..k).max_by_key(|&i| size[i]).unwrap_or(0);
    let mut keep: Vec<bool> = (0..k)
        .map(|i| i == biggest || size[i] as f64 >= MIN_VOICE_SHARE * sample.len() as f64)
        .collect();
    let cents = centroids(&sample, &sample_labels, k);
    let mut labels: Vec<usize> = points.iter().map(|p| nearest(p, &cents, &keep)).collect();
    for (&i, &l) in basis.iter().zip(&sample_labels) {
        if keep[l] {
            labels[i] = l;
        }
    }
    // Then fold the voice with the smallest share of the words' time until
    // none is small (a voice that hardly says anything readable).
    let word_labels = loop {
        let smoothed = smooth(windows, &labels);
        let wl = word_labels(words, windows, &smoothed);
        let mut time = vec![0f64; k];
        for (w, &l) in words.iter().zip(&wl) {
            time[l] += (w.end - w.start).max(0.01);
        }
        let total: f64 = time.iter().sum();
        let share = |i: usize| time[i] / total;
        let smallest = (0..k)
            .filter(|&i| keep[i])
            .min_by(|&a, &b| share(a).total_cmp(&share(b)));
        let alive = keep.iter().filter(|&&x| x).count();
        match smallest {
            Some(s) if alive > 1 && share(s) < MIN_VOICE_SHARE => {
                keep[s] = false;
                for (i, l) in labels.iter_mut().enumerate() {
                    if *l == s {
                        *l = nearest(&points[i], &cents, &keep);
                    }
                }
            }
            _ => break wl,
        }
    };
    // Number by first appearance in time.
    let mut first = vec![f64::INFINITY; k];
    for (w, &l) in words.iter().zip(&word_labels) {
        first[l] = first[l].min(w.start);
    }
    let mut order: Vec<usize> = (0..k).filter(|&i| first[i].is_finite()).collect();
    order.sort_by(|&a, &b| first[a].total_cmp(&first[b]));
    if order.len() < 2 {
        return 0;
    }
    let mut number = vec![0u32; k];
    for (i, &l) in order.iter().enumerate() {
        number[l] = i as u32 + 1;
    }
    for (w, &l) in words.iter_mut().zip(&word_labels) {
        w.voice = Some(number[l]);
    }
    order.len() as u32
}

/// Voices with less than `min_speech_s` seconds of words join the one
/// with the most words; numbers are given again by first appearance.
/// Returns the number of voices left (0 for one: no voice on any word).
pub fn fold_minor(words: &mut [Word], min_speech_s: f64) -> u32 {
    // (voice, words, seconds of its words)
    let mut count: Vec<(u32, usize, f64)> = Vec::new();
    for w in words.iter() {
        if let Some(v) = w.voice {
            let s = (w.end - w.start).max(0.0);
            match count.iter_mut().find(|(x, ..)| *x == v) {
                Some(c) => {
                    c.1 += 1;
                    c.2 += s;
                }
                None => count.push((v, 1, s)),
            }
        }
    }
    let Some(&(main, ..)) = count.iter().max_by_key(|(_, n, _)| *n) else {
        return 0;
    };
    let kept: Vec<u32> = count
        .iter()
        .filter(|(v, _, s)| *v == main || *s >= min_speech_s)
        .map(|(v, ..)| *v)
        .collect();
    // New numbers by first appearance of the voices kept.
    let mut order: Vec<u32> = Vec::new();
    for w in words.iter_mut() {
        if let Some(v) = w.voice {
            let v = if kept.contains(&v) { v } else { main };
            if !order.contains(&v) {
                order.push(v);
            }
            w.voice = Some(v);
        }
    }
    if order.len() < 2 {
        for w in words.iter_mut() {
            w.voice = None;
        }
        return 0;
    }
    for w in words.iter_mut() {
        if let Some(v) = w.voice {
            w.voice = order.iter().position(|&x| x == v).map(|i| i as u32 + 1);
        }
    }
    order.len() as u32
}

/// Majority of a window and its touching neighbours (a lone window between
/// two of another voice takes theirs).
fn smooth(windows: &[(u64, usize)], labels: &[usize]) -> Vec<usize> {
    let touch =
        |a: (u64, usize), b: (u64, usize)| b.0 <= a.0 + a.1 as u64 + SAMPLE_RATE as u64 / 20;
    (0..labels.len())
        .map(|i| {
            let prev = (i > 0 && touch(windows[i - 1], windows[i])).then(|| labels[i - 1]);
            let next =
                (i + 1 < labels.len() && touch(windows[i], windows[i + 1])).then(|| labels[i + 1]);
            match (prev, next) {
                (Some(p), Some(q)) if p == q => p,
                _ => labels[i],
            }
        })
        .collect()
}

/// Number of distinct labels (0..).
pub fn count(labels: &[usize]) -> usize {
    labels.iter().max().map_or(0, |m| m + 1)
}

/// The label of the windows around each word: the majority of the windows
/// containing its middle, else the nearest window close by, else the
/// previous word's.
fn word_labels(words: &[Word], windows: &[(u64, usize)], labels: &[usize]) -> Vec<usize> {
    let sr = SAMPLE_RATE as f64;
    let n = labels.len();
    let mids: Vec<f64> = windows
        .iter()
        .map(|&(s, l)| (s as f64 + l as f64 / 2.0) / sr)
        .collect();
    let mut last = labels.first().copied().unwrap_or(0);
    words
        .iter()
        .map(|w| {
            let t = (w.start + w.end) / 2.0;
            let hi = windows.partition_point(|&(s, _)| s as f64 / sr <= t);
            let mut votes: Vec<(usize, usize)> = Vec::new();
            let mut i = hi;
            while i > 0 {
                i -= 1;
                let (s, l) = windows[i];
                if (s as f64 / sr) < t - WINDOW_S - 0.01 {
                    break;
                }
                if (s + l as u64) as f64 / sr >= t {
                    match votes.iter_mut().find(|(lab, _)| *lab == labels[i]) {
                        Some(v) => v.1 += 1,
                        None => votes.push((labels[i], 1)),
                    }
                }
            }
            let label = if let Some(&(l, _)) = votes
                .iter()
                .max_by_key(|(l, c)| (*c, std::cmp::Reverse(*l)))
            {
                l
            } else {
                let j = mids.partition_point(|&m| m < t);
                [j.checked_sub(1), (j < n).then_some(j)]
                    .into_iter()
                    .flatten()
                    .map(|k| ((mids[k] - t).abs(), k))
                    .min_by(|a, b| a.0.total_cmp(&b.0))
                    .filter(|(d, _)| *d <= WINDOW_S)
                    .map_or(last, |(_, k)| labels[k])
            };
            last = label;
            label
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u64 = SAMPLE_RATE as u64;

    /// A unit vector near axis `axis` with a little deterministic noise.
    fn print(axis: usize, seed: u64) -> Vec<f32> {
        let mut v = vec![0f32; 16];
        v[axis] = 1.0;
        let mut x = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        for e in v.iter_mut() {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
            *e += ((x >> 33) as f32 / u32::MAX as f32 - 0.25) * 0.3;
        }
        normalized(&v)
    }

    #[test]
    fn minor_voices_fold_into_the_main_one() {
        // Each word lasts a second: voice 2 says 5 words first (5 s), voice
        // 1 the next 25 (25 s), voice 3 the last 30 (30 s, the most).
        let mut ws: Vec<Word> = (0..60)
            .map(|i| {
                let mut w = Word::new("слово", i as f64, i as f64 + 1.0);
                w.voice = Some(match i {
                    0..5 => 2,
                    5..30 => 1,
                    _ => 3,
                });
                w
            })
            .collect();
        assert_eq!(fold_minor(&mut ws, 25.0), 2);
        // Voice 2 joined the biggest (old 3), so it comes first now.
        assert_eq!(ws[0].voice, Some(1));
        assert_eq!(ws[10].voice, Some(2));
        assert_eq!(ws[40].voice, Some(1));
        // Nothing big enough besides the main voice: no voices at all.
        let mut few: Vec<Word> = ws.iter().take(35).cloned().collect();
        for (i, w) in few.iter_mut().enumerate() {
            w.voice = Some(if i < 30 { 1 } else { 2 });
        }
        assert_eq!(fold_minor(&mut few, 25.0), 0);
        assert!(few.iter().all(|w| w.voice.is_none()));
    }

    /// Words count less than time: the extra "voices" of one person on a
    /// call were dozens of short words in a few seconds (Odoo call: 31 and
    /// 38 words, 10–11 s), while a real second person at the microphone
    /// talks for minutes.
    #[test]
    fn many_short_words_are_not_a_person() {
        let mut ws: Vec<Word> = Vec::new();
        let mut at = 0.0;
        let mut say = |n: usize, len: f64, voice: u32, ws: &mut Vec<Word>| {
            for _ in 0..n {
                let mut w = Word::new("так", at, at + len);
                w.voice = Some(voice);
                ws.push(w);
                at += len + 0.1;
            }
        };
        say(377, 0.6, 1, &mut ws); // the user: ~226 s
        say(38, 0.3, 2, &mut ws); // replies in another tone: ~11 s
        say(135, 0.5, 3, &mut ws); // someone else in the room: ~68 s
        assert_eq!(fold_minor(&mut ws, MIC_MIN_SPEECH_S), 2);
        assert!(ws[377..415].iter().all(|w| w.voice == Some(1)));
        assert!(ws[415..].iter().all(|w| w.voice == Some(2)));
    }

    #[test]
    fn windows_cover_speech_and_skip_crumbs() {
        let w = windows(&[
            (0, 4 * SR as usize),
            (10 * SR, SR as usize),
            (20 * SR, 4000),
        ]);
        // 4 s: windows at 0, 0.75, 1.5, 2.25 and the tail at 2.5; 1 s: one.
        let starts: Vec<f64> = w.iter().map(|&(s, _)| s as f64 / SR as f64).collect();
        assert_eq!(starts, vec![0.0, 0.75, 1.5, 2.25, 2.5, 10.0]);
        assert_eq!(w[5].1, SR as usize);
    }

    #[test]
    fn clusters_match_average_linkage() {
        // Three tight groups; the threshold keeps them apart.
        let pts: Vec<Vec<f32>> = (0..30).map(|i| print(i % 3, i as u64)).collect();
        let l = cluster(&pts, THRESHOLD);
        assert_eq!(count(&l), 3);
        for i in 0..30 {
            assert_eq!(l[i], l[i % 3], "{l:?}");
        }
        // A huge threshold joins everything; a tiny one nothing.
        assert_eq!(count(&cluster(&pts, 2.0)), 1);
        assert_eq!(count(&cluster(&pts, 0.0)), 30);
    }

    /// One word per window, in the middle of it.
    fn words_for(w: &[(u64, usize)]) -> Vec<Word> {
        w.iter()
            .map(|&(s, l)| {
                let mid = (s as f64 + l as f64 / 2.0) / SR as f64;
                Word::new("w", mid - 0.1, mid + 0.1)
            })
            .collect()
    }

    #[test]
    fn voices_fold_crumbs_smooth_and_number_by_appearance() {
        // Voice B speaks first, then A; one stray window of a third voice
        // and one lone A window inside B.
        let mut w = Vec::new();
        let mut p = Vec::new();
        for i in 0..40u64 {
            w.push((i * SR, SR as usize));
            let axis = match i {
                5 => 0,  // lone A inside B: smoothed away
                12 => 7, // a crumb voice: folded
                0..20 => 1,
                _ => 0,
            };
            p.push(print(axis, i));
        }
        let mut words = words_for(&w);
        assert_eq!(assign(&mut words, &w, &p), 2);
        let v: Vec<u32> = words.iter().map(|w| w.voice.unwrap()).collect();
        assert!(v[..20].iter().all(|&x| x == 1), "{v:?}");
        assert!(v[20..].iter().all(|&x| x == 2), "{v:?}");
    }

    #[test]
    fn a_voice_without_words_is_not_counted() {
        // The second voice only on windows where nothing was recognized.
        let w: Vec<(u64, usize)> = (0..40u64).map(|i| (i * SR, SR as usize)).collect();
        let p: Vec<Vec<f32>> = (0..40)
            .map(|i| print(if i < 30 { 1 } else { 5 }, i))
            .collect();
        let mut words = words_for(&w[..30]);
        assert_eq!(assign(&mut words, &w, &p), 0);
        assert!(words.iter().all(|w| w.voice.is_none()));
    }

    #[test]
    fn many_windows_are_clustered_on_a_sample() {
        let n = MAX_CLUSTER_POINTS + 500;
        let w: Vec<(u64, usize)> = (0..n as u64).map(|i| (i * SR, SR as usize)).collect();
        let p: Vec<Vec<f32>> = (0..n)
            .map(|i| print(if i < n / 2 { 2 } else { 4 }, i as u64))
            .collect();
        let mut words = words_for(&w);
        assert_eq!(assign(&mut words, &w, &p), 2);
        assert!(words[..n / 2 - 1].iter().all(|w| w.voice == Some(1)));
        assert!(words[n / 2 + 1..].iter().all(|w| w.voice == Some(2)));
    }

    #[test]
    fn words_take_the_voice_around_them() {
        let w: Vec<(u64, usize)> = vec![(0, 3 * SR as usize), (10 * SR, 2 * SR as usize)];
        let labels = vec![0, 1];
        let words = vec![
            Word::new("a", 1.0, 1.2),   // inside the first window
            Word::new("b", 10.5, 10.7), // inside the second
            Word::new("c", 12.6, 12.8), // just after it: nearest
            Word::new("d", 30.0, 30.2), // far away: the previous word's
        ];
        assert_eq!(word_labels(&words, &w, &labels), vec![0, 1, 1, 1]);
    }
}
