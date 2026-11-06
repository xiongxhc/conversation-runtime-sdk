//! Text rules for speaker identity: the key a name is compared by, the cheap cue that a
//! turn may carry a self-introduction, and the yes or no that answers an identity question.

use pinyin::ToPinyin;

const CHINESE_CUES: [&str; 3] = ["我是", "我叫", "我的名字是"];
const ENGLISH_CUES: [&[&str]; 4] = [
    &["i'm"],
    &["i", "am"],
    &["my", "name", "is"],
    &["call", "me"],
];
const YES: [&str; 17] = [
    "是",
    "是的",
    "是我",
    "对",
    "对的",
    "对啊",
    "嗯",
    "嗯嗯",
    "没错",
    "yes",
    "yeah",
    "yep",
    "yup",
    "correct",
    "right",
    "that's me",
    "it's me",
];
const NO: [&str; 7] = ["不是", "不对", "不是我", "没有", "no", "nope", "not me"];
const ANSWER_TRIM: [char; 8] = ['。', '，', '！', '？', '.', ',', '!', '?'];

/// The form two names are compared in: Chinese characters by toneless pinyin, so names
/// written with different characters for the same sound match, and everything else by
/// lowercase letters and digits. `None` when the name has nothing comparable in it.
pub fn name_key(name: &str) -> Option<String> {
    let mut key = String::new();
    for character in name.chars() {
        if let Some(reading) = character.to_pinyin() {
            key.push_str(reading.plain());
        } else if character.is_alphanumeric() {
            key.extend(character.to_lowercase());
        }
    }
    (!key.is_empty()).then_some(key)
}

/// Whether a turn may contain a self-introduction. A hit only means the turn is worth
/// asking the language model about; it does not say what the name is.
pub fn has_name_claim_cue(transcript: &str) -> bool {
    if CHINESE_CUES.iter().any(|cue| transcript.contains(cue)) {
        return true;
    }
    let lowered = transcript.to_lowercase().replace('’', "'");
    let words: Vec<&str> = lowered
        .split(|character: char| !(character.is_alphabetic() || character == '\''))
        .filter(|word| !word.is_empty())
        .collect();
    ENGLISH_CUES
        .iter()
        .any(|cue| words.windows(cue.len()).any(|window| window == *cue))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentityAnswer {
    Yes,
    No,
}

/// Reads a whole utterance as the answer to "is that you?". Anything longer or vaguer
/// than a plain yes or no is not an answer.
pub fn identity_answer(transcript: &str) -> Option<IdentityAnswer> {
    let lowered = transcript.to_lowercase().replace('’', "'");
    let answer = lowered.trim().trim_matches(|character: char| {
        character.is_whitespace() || ANSWER_TRIM.contains(&character) || character == '~'
    });
    if YES.contains(&answer) {
        Some(IdentityAnswer::Yes)
    } else if NO.contains(&answer) {
        Some(IdentityAnswer::No)
    } else {
        None
    }
}
