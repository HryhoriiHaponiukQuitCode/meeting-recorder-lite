//! Names of the people on a call, from how they address each other.
//!
//! Every participant (the microphone, and each voice of the system audio)
//! is called by name when the conversation makes it clear, else
//! "Participant N". The evidence, on the turns of the dialogue:
//!
//! - **Hand-over:** a name addressed near the end of a turn ("…Каті,
//!   розкажи, що тут вдалося") points at whoever speaks next.
//! - **Thanks:** "Дякую, Роман, за коментар" / "Thanks, Sally" points at
//!   whoever spoke just before (and also at the next one when the turn
//!   ends with a question: "Дякую, Григорій. Ти хочеш щось задати?").
//! - **Introduction:** "мене звати Оля", "I'm John", "this is John".
//! - Whoever says a name to someone is not that person.
//!
//! A name is given only when one participant clearly leads for it and it
//! clearly leads for that participant (see [`MIN_SCORE`], [`MARGIN`]); a
//! wrong name is worse than a number. Names are recognized from a list of
//! Ukrainian and English first names in any case form ("Каті", "Романе",
//! "Оленко") and written in the form of the list ("Катя", "Роман",
//! "Оленка").

use std::collections::HashMap;

use crate::Speaker;
use crate::transcript::Utterance;

/// Points needed for a name.
pub const MIN_SCORE: i32 = 2;
/// The winner must lead the runner-up by this much, both ways.
pub const MARGIN: i32 = 2;
/// A turn this close before or after counts as the one addressed.
const NEIGHBOUR_S: f64 = 15.0;
/// Hand-over: the name is in one of the last this many sentences of a turn.
const LAST_SENTENCES: usize = 2;

/// Who speaks: the channel and, on the system audio, the voice.
pub type Key = (Speaker, Option<u32>);

/// First names, nominative; diminutives that people say as names are
/// entries of their own ("Оленка", "Катя").
const UA_NAMES: &str = "Андрій Антон Артем Артур Богдан Борис Вадим Валентин Валерій Василь \
Віктор Віталій Владислав Влад Володимир Вова В'ячеслав Слава Геннадій Гліб Григорій Гриша \
Давид Данило Денис Дмитро Діма Євген Женя Єгор Захар Зіновій Іван Ваня Ігор Ілля Йосип \
Кирило Костянтин Костя Максим Макс Марко Матвій Микита Микола Михайло Мирослав Назар \
Олег Олександр Саша Олексій Льоша Остап Павло Петро Роман Рома Ростислав Руслан Сергій \
Сергій Сєрьожа Станіслав Стас Степан Тарас Тимофій Тимур Федір Юрій Юра Ярослав Ярик \
Аліна Алла Анастасія Настя Ангеліна Анна Аня Ганна Антоніна Валентина Валерія Леря \
Вероніка Віка Вікторія Віра Галина Галя Дарина Даша Діана Єва Євгенія Єлизавета Ліза \
Зоряна Зоя Ірина Іра Ярина Карина Катерина Катя Кіра Христина Лариса Леся Лілія Любов \
Люба Людмила Мар'яна Марина Марія Маша Мілана Надія Наталія Наталя Наташа Ніна Оксана \
Олена Оленка Лєна Ольга Оля Поліна Раїса Світлана Світла Софія Соня Тамара Тетяна Таня \
Уляна Юлія Юля Яна Ярослава";

const EN_NAMES: &str = "Aaron Adam Adrian Alan Albert Alex Alexander Andrew Andy Anthony Ben \
Benjamin Bill Bob Brad Brandon Brian Bruce Carl Charles Charlie Chris Christian Christopher \
Daniel Dan Danny Dave David Dennis Derek Doug Douglas Ed Edward Eric Ethan Frank Gary George \
Greg Henry Ian Jack Jacob Jake James Jason Jeff Jeffrey Jeremy Jim Joe John Johnny Jon \
Jonathan Jordan Joseph Josh Joshua Justin Keith Ken Kevin Kyle Larry Leo Luke Mark Matt \
Matthew Max Michael Mike Nathan Nick Noah Oliver Patrick Paul Peter Phil Ray Richard Rick \
Rob Robert Ron Ryan Sam Scott Sean Simon Steve Steven Ted Thomas Tim Tom Tony Tyler Victor \
Walter William Will Abigail Alice Alicia Amanda Amber Amy Andrea Angela Anna Anne Ashley \
Barbara Betty Beth Brenda Carol Caroline Catherine Cathy Charlotte Cheryl Christina Christine \
Claire Cindy Daisy Dana Deborah Debbie Diana Diane Donna Dorothy Elena Elizabeth Ellen Emily \
Emma Erin Eva Grace Hannah Heather Helen Jane Janet Jennifer Jenny Jessica Jill Joan Joanna \
Julia Julie Karen Kate Katie Kathy Kelly Kim Laura Lauren Linda Lisa Liz Lucy Madison Maria \
Marie Mary Megan Melissa Michelle Monica Nancy Natalie Nicole Olivia Pam Pamela Patricia \
Rachel Rebecca Rose Ruth Sally Samantha Sandra Sara Sarah Sharon Sophia Sophie Stephanie \
Susan Teresa Tina Tracy Victoria Wendy";

/// Words of thanks that point at the previous speaker.
const THANKS: &[&str] = &["дякую", "дякуємо", "спасибі", "дяка", "thanks", "thank"];

/// A name list entry: the written form and the stem its case forms share.
struct Entry {
    name: &'static str,
    stem: String,
}

fn lower(s: &str) -> String {
    s.chars().flat_map(char::to_lowercase).collect()
}

fn entries() -> Vec<Entry> {
    UA_NAMES
        .split_whitespace()
        .chain(EN_NAMES.split_whitespace())
        .map(|name| {
            let l = lower(name);
            // Ukrainian case endings replace the final vowel / й / ь.
            let stem = if name.chars().next().is_some_and(|c| c.is_ascii()) {
                l
            } else {
                l.trim_end_matches(['а', 'я', 'о', 'й', 'ь', 'є'])
                    .to_string()
            };
            Entry { name, stem }
        })
        .collect()
}

/// The listed name a capitalized word is a form of ("Каті" → "Катя"),
/// preferring the longest stem ("Оленко" → "Оленка", not "Олена").
fn name_of(word: &str, list: &[Entry]) -> Option<&'static str> {
    let first = word.chars().next()?;
    if !first.is_uppercase() {
        return None;
    }
    let w = lower(word);
    let ascii = first.is_ascii();
    list.iter()
        .filter(|e| {
            if ascii {
                // English names take no endings; "Sally's" is still Sally.
                w == e.stem || w == format!("{}'s", e.stem) || w == format!("{}’s", e.stem)
            } else {
                // Short stems ("Ол" of "Оля") take one-letter endings only.
                let stem = e.stem.chars().count();
                let max_ending = if stem >= 3 { 3 } else { 1 };
                stem >= 2 && w.starts_with(&e.stem) && w.chars().count() - stem <= max_ending
            }
        })
        .max_by_key(|e| e.stem.chars().count())
        .map(|e| e.name)
}

/// A name said in a sentence, and whether it is addressed (set off by
/// punctuation or at the start, as a vocative is).
struct Mention {
    name: &'static str,
    sentence: usize,
    addressed: bool,
    /// "мене звати Оля": the speaker's own name.
    intro: bool,
}

fn sentences(text: &str) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = vec![Vec::new()];
    for raw in text.split_whitespace() {
        if let Some(s) = out.last_mut() {
            s.push(raw.to_string());
        }
        if raw.ends_with(['.', '?', '!', '…']) {
            out.push(Vec::new());
        }
    }
    out.retain(|s| !s.is_empty());
    out
}

fn bare(raw: &str) -> &str {
    raw.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'' && c != '’')
}

/// One speaker's consecutive utterances.
struct Turn {
    key: Key,
    start: f64,
    end: f64,
    text: String,
}

fn turns(dialog: &[Utterance]) -> Vec<Turn> {
    let mut out: Vec<Turn> = Vec::new();
    for u in dialog.iter().filter(|u| !u.echo) {
        let key = (u.speaker, u.voice);
        match out.last_mut() {
            Some(t) if t.key == key => {
                t.text.push(' ');
                t.text.push_str(&u.text);
                t.end = t.end.max(u.end);
            }
            _ => out.push(Turn {
                key,
                start: u.start,
                end: u.end,
                text: u.text.clone(),
            }),
        }
    }
    out
}

const INTRO_UA: &[&str] = &["звати", "звуть"];
const INTRO_EN: &[(&str, &str)] = &[("i'm", ""), ("i", "am"), ("this", "is"), ("name", "is")];

/// Names for the participants of `dialog` (echo left out): `key → name`
/// for those the conversation makes clear.
pub fn find(dialog: &[Utterance]) -> HashMap<Key, String> {
    let list = entries();
    let turns = turns(dialog);
    let mut score: HashMap<(Key, &'static str), i32> = HashMap::new();
    let mut add = |key: Key, name: &'static str, n: i32| {
        *score.entry((key, name)).or_default() += n;
    };
    for (i, t) in turns.iter().enumerate() {
        let sents = sentences(&t.text);
        let mut mentions = Vec::new();
        for (si, s) in sents.iter().enumerate() {
            for (wi, raw) in s.iter().enumerate() {
                let Some(name) = name_of(bare(raw), &list) else {
                    continue;
                };
                // A vocative is set off by commas ("Каті, розкажи", "Дякую,
                // Роман, за…", "…, Оленко."); a name before a full stop
                // alone is usually told about ("про них говорив Денис.").
                let addressed = raw.ends_with(',')
                    || (wi > 0 && s[wi - 1].ends_with(','))
                    || (wi == 0 && raw.ends_with(['!', '?']));
                // Introductions: "мене звати Оля", "I'm John", "this is John".
                let prev = |k: usize| wi.checked_sub(k).map(|j| lower(bare(&s[j])));
                let intro_ua = prev(1).is_some_and(|p| INTRO_UA.contains(&p.as_str()));
                let intro_en = INTRO_EN.iter().any(|(a, b)| {
                    if b.is_empty() {
                        prev(1).as_deref() == Some(*a)
                    } else {
                        prev(2).as_deref() == Some(*a) && prev(1).as_deref() == Some(*b)
                    }
                });
                let intro = intro_ua || intro_en;
                if intro {
                    add(t.key, name, 3);
                }
                mentions.push(Mention {
                    name,
                    sentence: si,
                    addressed,
                    intro,
                });
            }
        }
        let ends_with_question = t.text.trim_end().ends_with('?');
        for m in mentions.iter().filter(|m| !m.intro) {
            // Nobody addresses themselves.
            add(t.key, m.name, -3);
            let crowded = mentions
                .iter()
                .filter(|o| o.sentence == m.sentence && o.name != m.name)
                .count()
                > 0;
            if !m.addressed || crowded {
                continue;
            }
            // Thanks to the previous one ("Дякую, Роман, за коментар"); a
            // hand-over to the next one otherwise, or when the turn ends
            // with a question ("Дякую, Григорій. Ти хочеш щось задати?").
            let thanks = sents[m.sentence]
                .iter()
                .any(|w| THANKS.contains(&lower(bare(w)).as_str()));
            if (!thanks || ends_with_question)
                && m.sentence + LAST_SENTENCES >= sents.len()
                && let Some(next) = turns.get(i + 1)
                && next.key != t.key
                && next.start - t.end <= NEIGHBOUR_S
            {
                add(next.key, m.name, 2);
            }
            if thanks
                && let Some(prev) = i.checked_sub(1).and_then(|p| turns.get(p))
                && prev.key != t.key
                && t.start - prev.end <= NEIGHBOUR_S
            {
                add(prev.key, m.name, 2);
            }
        }
    }
    assign(score)
}

/// Gives each name to at most one participant and back, only where both
/// lead clearly.
fn assign(mut score: HashMap<(Key, &'static str), i32>) -> HashMap<Key, String> {
    let mut out = HashMap::new();
    loop {
        let best = score
            .iter()
            .filter(|(_, s)| **s >= MIN_SCORE)
            .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.1.cmp(a.0.1)))
            .map(|(k, s)| (*k, *s));
        let Some(((key, name), s)) = best else {
            break;
        };
        let other_for_name = score
            .iter()
            .filter(|((k, n), _)| *n == name && *k != key)
            .map(|(_, v)| *v)
            .max()
            .unwrap_or(0);
        let other_for_key = score
            .iter()
            .filter(|((k, n), _)| *k == key && *n != name)
            .map(|(_, v)| *v)
            .max()
            .unwrap_or(0);
        let name_clear = s - other_for_name >= MARGIN;
        let key_clear = s - other_for_key >= MARGIN;
        if name_clear && key_clear {
            out.insert(key, name.to_string());
        }
        // Settled: a given name and its participant; else what is unclear
        // (a name two people may have, a participant with two names).
        let drop_key = key_clear == name_clear || !key_clear;
        let drop_name = key_clear == name_clear || !name_clear;
        score.retain(|(k, n), _| !(drop_key && *k == key) && !(drop_name && *n == name));
    }
    out
}

/// Labels of everyone in `dialog`: the name, or "Participant N" numbered
/// in order of appearance among those without one.
pub fn labels(dialog: &[Utterance], names: &HashMap<Key, String>) -> Vec<(Key, String)> {
    let mut out: Vec<(Key, String)> = Vec::new();
    let mut n = 0;
    for u in dialog.iter().filter(|u| !u.echo) {
        let key = (u.speaker, u.voice);
        if out.iter().any(|(k, _)| *k == key) {
            continue;
        }
        let label = match names.get(&key) {
            Some(name) => name.clone(),
            None => {
                n += 1;
                format!("Participant {n}")
            }
        };
        out.push((key, label));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(speaker: Speaker, voice: Option<u32>, start: f64, text: &str) -> Utterance {
        Utterance {
            speaker,
            start,
            end: start + 5.0,
            text: text.into(),
            words: vec![],
            echo: false,
            voice,
        }
    }

    const O: Speaker = Speaker::Others;
    const ME: Speaker = Speaker::Me;

    #[test]
    fn case_forms_map_to_the_listed_name() {
        let l = entries();
        assert_eq!(name_of("Каті", &l), Some("Катя"));
        assert_eq!(name_of("Романе", &l), Some("Роман"));
        assert_eq!(name_of("Оленко", &l), Some("Оленка"));
        assert_eq!(name_of("Олено", &l), Some("Олена"));
        assert_eq!(name_of("Тарасе", &l), Some("Тарас"));
        assert_eq!(name_of("Григорію", &l), Some("Григорій"));
        assert_eq!(name_of("Sally's", &l), Some("Sally"));
        assert_eq!(name_of("Олю", &l), Some("Оля"));
        assert_eq!(name_of("Олег", &l), Some("Олег"));
        assert_eq!(name_of("Олексій", &l), Some("Олексій"));
        assert_eq!(name_of("роман", &l), None, "a common noun");
        assert_eq!(name_of("Романтика", &l), None, "too long an ending");
        assert_eq!(name_of("Salvation", &l), None);
    }

    /// The pattern of a real meeting: the host hands over and thanks.
    #[test]
    fn hand_over_and_thanks_name_the_speakers() {
        let d = vec![
            u(
                O,
                Some(1),
                0.0,
                "Фільтрація case study. Тому, будь ласка, Каті, розкажи, що тут вдалося.",
            ),
            u(
                O,
                Some(2),
                6.0,
                "Насправді тут просто була задача розподілити кейси.",
            ),
            u(
                O,
                Some(1),
                12.0,
                "Дякую, Каті. Тепер послухаємо Оленку. Оленко, розкажи, будь ласка.",
            ),
            u(
                O,
                Some(3),
                18.0,
                "Так, була ідея подаватися з інших профілів.",
            ),
            u(O, Some(1), 24.0, "Добре, дякую, Оленко. Рухаємося далі."),
            u(
                O,
                Some(4),
                30.0,
                "Тобто більшість помилок можна було уникнути.",
            ),
            u(
                O,
                Some(1),
                36.0,
                "Дякую, Роман, за коментар. Наступна ініціатива.",
            ),
            u(ME, None, 42.0, "У мене є питання."),
        ];
        let names = find(&d);
        assert_eq!(names.get(&(O, Some(2))).map(String::as_str), Some("Катя"));
        assert_eq!(names.get(&(O, Some(3))).map(String::as_str), Some("Оленка"));
        assert_eq!(names.get(&(O, Some(4))).map(String::as_str), Some("Роман"));
        assert_eq!(names.get(&(O, Some(1))), None, "the host is never named");
        assert_eq!(names.get(&(ME, None)), None);
        let l = labels(&d, &names);
        assert_eq!(l[0].1, "Participant 1");
        assert_eq!(l[1].1, "Катя");
        assert_eq!(l.last().unwrap().1, "Participant 2");
    }

    #[test]
    fn introductions_and_english() {
        let d = vec![
            u(
                O,
                Some(1),
                0.0,
                "Hi everyone, I'm Sally from the donor team.",
            ),
            u(ME, None, 5.0, "Nice to meet you. Thanks, Sally."),
            u(O, Some(2), 10.0, "Привіт, мене звати Оля."),
        ];
        let names = find(&d);
        assert_eq!(names.get(&(O, Some(1))).map(String::as_str), Some("Sally"));
        assert_eq!(names.get(&(O, Some(2))).map(String::as_str), Some("Оля"));
        assert_eq!(names.get(&(ME, None)), None);
    }

    #[test]
    fn unclear_or_conflicting_evidence_gives_no_name() {
        // "Дякую, Григорій. Ти хочеш щось задати?": thanks to the one
        // before, or hand-over to the one after? Equal points: no name.
        let d = vec![
            u(O, Some(2), 0.0, "Я там ще казала про таблицю."),
            u(
                O,
                Some(1),
                6.0,
                "Добре, дякую, Григорій. Ти хочеш щось задати?",
            ),
            u(ME, None, 12.0, "Тут загалом по Upwork."),
            // Several names in one sentence prove nothing.
            u(O, Some(1), 18.0, "Дякую, Григорій, Оленка та Каті."),
            // A name not set off as an address: just talk about someone.
            u(O, Some(3), 24.0, "Подавалися з профілю Романа."),
            u(
                O,
                Some(1),
                30.0,
                "Наскільки пам'ятаю, про них говорив Денис.",
            ),
            u(O, Some(4), 34.0, "Так, можна тут уточнити."),
        ];
        let names = find(&d);
        assert!(names.is_empty(), "{names:?}");
        let l = labels(&d, &names);
        let all: Vec<&str> = l.iter().map(|x| x.1.as_str()).collect();
        assert_eq!(
            all,
            vec![
                "Participant 1",
                "Participant 2",
                "Participant 3",
                "Participant 4",
                "Participant 5"
            ]
        );
    }
}
