use conversation_runtime::{has_name_claim_cue, identity_answer, name_key, IdentityAnswer};

#[test]
fn chinese_names_with_different_characters_share_a_key() {
    assert_eq!(name_key("昊辰"), Some("haochen".to_owned()));
    assert_eq!(name_key("浩晨"), Some("haochen".to_owned()));
}

#[test]
fn latin_names_are_case_folded_and_stripped() {
    assert_eq!(name_key("Jack"), Some("jack".to_owned()));
    assert_eq!(name_key(" JACK "), Some("jack".to_owned()));
    assert_eq!(name_key("O'Brien"), Some("obrien".to_owned()));
    assert_eq!(name_key("陈 Chris"), Some("chenchris".to_owned()));
    assert_eq!(name_key(" ,.! "), None);
    assert_eq!(name_key(""), None);
}

#[test]
fn claim_cues_in_both_languages() {
    for text in [
        "我是Jack",
        "你好，我叫昊辰",
        "我的名字是小明",
        "I'm Jack",
        "i am jack",
        "Hi, my name is Jack",
        "Call me Jack",
        "I’m Jack",
    ] {
        assert!(has_name_claim_cue(text), "{text}");
    }
    assert!(!has_name_claim_cue("what's the weather"));
}

#[test]
fn english_cues_need_word_boundaries() {
    for text in [
        "hi amy",
        "Miami is warm",
        "recall meetings",
        "xi am",
        "im here",
    ] {
        assert!(!has_name_claim_cue(text), "{text}");
    }
}

#[test]
fn yes_and_no_answers_in_both_languages() {
    for text in ["是", "是的。", "对啊！", "嗯", "Yes.", "yeah", "That’s me"] {
        assert_eq!(identity_answer(text), Some(IdentityAnswer::Yes), "{text}");
    }
    for text in ["不是", "不对", "No!", "nope", "not me"] {
        assert_eq!(identity_answer(text), Some(IdentityAnswer::No), "{text}");
    }
}

#[test]
fn anything_else_is_not_an_answer() {
    for text in ["是不是", "yes and no", "I think so", "", "我是Jack"] {
        assert_eq!(identity_answer(text), None, "{text}");
    }
}
