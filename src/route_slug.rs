use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

pub(crate) fn is_valid(value: &str) -> bool {
    let bytes = value.as_bytes();
    (1..=48).contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

pub(crate) fn normalize(value: &str) -> String {
    let mut slug = String::new();
    let mut separator = false;
    for character in value
        .nfkd()
        .filter(|character| character.is_ascii() && !is_combining_mark(*character))
    {
        append_character(&mut slug, &mut separator, character);
    }
    slug
}

fn append_character(slug: &mut String, separator: &mut bool, character: char) {
    if !character.is_ascii_alphanumeric() {
        *separator = true;
        return;
    }
    if *separator && !slug.is_empty() {
        if slug.len() >= 47 {
            return;
        }
        slug.push('-');
    }
    *separator = false;
    if slug.len() < 48 {
        slug.push(character.to_ascii_lowercase());
    }
}
