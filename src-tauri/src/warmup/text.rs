//! Matching the words of a page: a button's name, a link's text, a banner's
//! wording, in any of the languages the warm-up knows.
//!
//! Text is lowercased and cut into words at anything that is not a letter or
//! digit (an apostrophe stays, so `j'accepte` is one word). A phrase made of
//! such words matches when its words appear next to each other. Scripts written
//! without spaces (Chinese, Japanese) and the agglutinative Korean match as a
//! plain substring instead, because there is no word boundary to look for.

/// Whether `c` belongs to a script that is matched by substring.
fn substring_script(c: char) -> bool {
  matches!(c,
    '\u{3040}'..='\u{30FF}'
    | '\u{3400}'..='\u{4DBF}'
    | '\u{4E00}'..='\u{9FFF}'
    | '\u{F900}'..='\u{FAFF}'
    | '\u{AC00}'..='\u{D7AF}'
    | '\u{FF66}'..='\u{FF9F}')
}

fn is_combining_mark(c: char) -> bool {
  ('\u{0300}'..='\u{036F}').contains(&c)
}

/// The text lowercased, with every run of anything but letters, digits and
/// apostrophes made a single space, and combining marks dropped.
pub fn normalize(text: &str) -> String {
  let mut out = String::with_capacity(text.len());
  let mut gap = true;
  for c in text.chars() {
    let c = match c {
      '\u{2019}' | '\u{2018}' | '\u{02BC}' | '\u{00B4}' | '`' => '\'',
      other => other,
    };
    if is_combining_mark(c) {
      continue;
    }
    if c.is_alphanumeric() || c == '\'' {
      // Lowercasing can add a mark of its own (a Turkish capital dotted I
      // becomes `i` and a combining dot).
      for lower in c.to_lowercase().filter(|lower| !is_combining_mark(*lower)) {
        out.push(lower);
      }
      gap = false;
    } else if !gap {
      out.push(' ');
      gap = true;
    }
  }
  while out.ends_with(' ') {
    out.pop();
  }
  out
}

/// Whether the normalized `text` holds the normalized `phrase`.
pub fn has_phrase(text: &str, phrase: &str) -> bool {
  if phrase.is_empty() || text.is_empty() {
    return false;
  }
  if phrase.chars().any(substring_script) {
    return text.contains(phrase);
  }
  let padded_text = format!(" {text} ");
  padded_text.contains(&format!(" {phrase} "))
}

/// Whether the normalized `text` holds any of `phrases`.
pub fn has_any(text: &str, phrases: &[&str]) -> bool {
  phrases
    .iter()
    .any(|phrase| has_phrase(text, &normalize(phrase)))
}

/// Whether the normalized `text` is exactly one of `phrases`.
pub fn is_any(text: &str, phrases: &[&str]) -> bool {
  phrases.iter().any(|phrase| text == normalize(phrase))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn text_is_lowercased_and_cut_at_punctuation() {
    assert_eq!(
      normalize("  Accept ALL   Cookies! \u{203A}"),
      "accept all cookies"
    );
    assert_eq!(normalize("J\u{2019}accepte"), "j'accepte");
    assert_eq!(normalize("\u{2713} OK"), "ok");
    assert_eq!(normalize(""), "");
    assert_eq!(normalize("!!!"), "");
  }

  #[test]
  fn combining_marks_do_not_break_a_word() {
    // Turkish capital dotted I lowercases to i plus a combining dot.
    assert_eq!(normalize("KABUL ED\u{0130}YORUM"), "kabul ediyorum");
  }

  #[test]
  fn words_match_whole_and_scripts_without_spaces_match_inside() {
    assert!(has_phrase(&normalize("Yes, I agree to all"), "i agree"));
    assert!(!has_phrase(&normalize("Disagree"), "agree"));
    assert!(!has_phrase(&normalize("Cartridge"), "cart"));
    assert!(has_phrase(&normalize("全部接受并关闭"), "全部接受"));
    assert!(has_phrase(&normalize("모든 쿠키 허용합니다"), "쿠키 허용"));
    assert!(!has_phrase("", "x"));
    assert!(!has_phrase("x", ""));
  }

  #[test]
  fn exact_means_the_whole_label() {
    assert!(is_any(&normalize("OK!"), &["ok", "okay"]));
    assert!(!is_any(&normalize("OK, manage"), &["ok", "okay"]));
    assert!(has_any(&normalize("Manage settings"), &["settings"]));
  }
}
