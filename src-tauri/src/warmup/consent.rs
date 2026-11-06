//! Telling a cookie-consent button from every other button on a page.
//!
//! Accepting the banner is what makes a site's cookies stay, so the warm-up
//! clicks it, with the mouse, like a visitor would. The rule is conservative on
//! purpose, because a wrong click is worse than a missed one:
//!
//! - the button must sit inside something that looks like a consent banner (a
//!   dialog or a fixed bar) whose text or ids talk about cookies or privacy;
//! - its name must say "accept / agree / allow" in one of the supported
//!   languages, in the whole or as a phrase;
//! - it must not also say reject, settings, "only necessary", save, learn more,
//!   or any login, purchase or deletion word.

use super::pointer::Rect;
use super::targets::forbidden_action;
use super::text::{has_any, is_any, normalize};
use serde::Deserialize;

/// A name that accepts the banner as a phrase: it counts wherever it appears
/// in the label ("Accept all cookies and continue").
const ACCEPT_PHRASES: &[&str] = &[
  // en
  "accept all",
  "accept cookies",
  "accept & close",
  "accept and close",
  "accept & continue",
  "accept and continue",
  "accept and proceed",
  "allow all",
  "allow cookies",
  "agree & continue",
  "agree and continue",
  "agree & close",
  "agree and close",
  "agree to all",
  "yes i agree",
  "i agree",
  "i accept",
  "i consent",
  "consent and continue",
  "got it",
  // vi
  "chấp nhận tất cả",
  "chấp nhận cookie",
  "đồng ý tất cả",
  "tôi đồng ý",
  "cho phép tất cả",
  "chấp nhận và đóng",
  "đồng ý và tiếp tục",
  // de
  "alle akzeptieren",
  "alles akzeptieren",
  "alle cookies akzeptieren",
  "akzeptieren und schließen",
  "akzeptieren und weiter",
  "alle zulassen",
  "alle cookies zulassen",
  "ich stimme zu",
  "zustimmen und weiter",
  "alle annehmen",
  // fr
  "tout accepter",
  "accepter tout",
  "accepter tous les cookies",
  "accepter et fermer",
  "accepter et continuer",
  "j'accepte",
  "autoriser tous les cookies",
  "tout autoriser",
  "accepter les cookies",
  // es
  "aceptar todo",
  "aceptar todas",
  "aceptar cookies",
  "aceptar y cerrar",
  "aceptar y continuar",
  "permitir todas",
  "permitir todo",
  "permitir cookies",
  "estoy de acuerdo",
  "acepto todo",
  // pt
  "aceitar tudo",
  "aceitar todos",
  "aceitar cookies",
  "aceitar e fechar",
  "aceitar e continuar",
  "permitir todos",
  "permitir tudo",
  "concordar e continuar",
  "concordo e continuar",
  "aceito todos",
  "eu concordo",
  // it
  "accetta tutto",
  "accetta tutti",
  "accetta e chiudi",
  "accetta e continua",
  "accetta i cookie",
  "accetto tutto",
  "consenti tutti",
  "consenti tutto",
  "permetti tutti",
  "acconsento",
  // ja
  "すべて同意",
  "全て同意",
  "すべて許可",
  "全て許可",
  "すべて受け入れ",
  "すべてのcookieを許可",
  "すべてのクッキーを許可",
  "同意して続行",
  "同意して閉じる",
  "同意する",
  "同意します",
  "承諾する",
  "受け入れる",
  "許可する",
  // ko
  "모두 수락",
  "모두 동의",
  "모두 허용",
  "모든 쿠키 허용",
  "모든 쿠키 수락",
  "동의합니다",
  "동의하고 계속",
  "수락하고 계속",
  "수락합니다",
  "쿠키 허용",
  "쿠키 수락",
  // zh
  "全部接受",
  "接受全部",
  "接受所有",
  "全部同意",
  "同意全部",
  "全部允许",
  "允许全部",
  "允许所有",
  "全部允許",
  "允許所有",
  "接受并关闭",
  "接受並關閉",
  "接受并继续",
  "接受並繼續",
  "同意并继续",
  "同意並繼續",
  "接受cookie",
  "我同意",
  "我接受",
  // ru
  "принять все",
  "принять всё",
  "принять и закрыть",
  "принять и продолжить",
  "принять файлы cookie",
  "принять cookie",
  "разрешить все",
  "разрешить всё",
  "я согласен",
  "я принимаю",
  // tr
  "tümünü kabul et",
  "tümüne izin ver",
  "hepsini kabul et",
  "hepsine izin ver",
  "tüm çerezleri kabul et",
  "tüm çerezlere izin ver",
  "çerezleri kabul et",
  "kabul et ve kapat",
  "kabul et ve devam et",
  "kabul ediyorum",
  "tümünü onayla",
];

/// A name that accepts only when it is the whole label: short words that would
/// match far too much inside a longer one.
const ACCEPT_EXACT: &[&str] = &[
  // en
  "accept",
  "agree",
  "allow",
  "agreed",
  "ok",
  "okay",
  "i understand",
  "understood",
  "continue",
  "yes",
  "accept cookie",
  "alright",
  // vi
  "chấp nhận",
  "đồng ý",
  "đã hiểu",
  "tôi hiểu",
  // de
  "akzeptieren",
  "zustimmen",
  "einverstanden",
  "verstanden",
  "annehmen",
  "alles klar",
  "zulassen",
  // fr
  "accepter",
  "d'accord",
  "compris",
  "autoriser",
  "j'ai compris",
  // es
  "aceptar",
  "acepto",
  "de acuerdo",
  "entendido",
  "vale",
  "permitir",
  // pt
  "aceitar",
  "aceito",
  "concordo",
  "entendi",
  "de acordo",
  "tudo bem",
  // it
  "accetta",
  "accetto",
  "ho capito",
  "va bene",
  "consenti",
  "d'accordo",
  // ja
  "同意",
  "許可",
  "承諾",
  "了解",
  "了解しました",
  // ko
  "수락",
  "동의",
  "허용",
  "확인",
  "알겠습니다",
  // zh
  "接受",
  "同意",
  "允许",
  "允許",
  "知道了",
  "我知道了",
  "好的",
  // ru
  "принять",
  "принимаю",
  "согласен",
  "согласиться",
  "понятно",
  "хорошо",
  "ок",
  "разрешить",
  // tr
  "kabul et",
  "kabul",
  "onayla",
  "anladım",
  "tamam",
  "izin ver",
];

/// Words that turn an accept-looking label into something else: a refusal, a
/// narrower accept, a settings door or a link to read more.
const NOT_ACCEPT: &[&str] = &[
  // en
  "reject",
  "decline",
  "deny",
  "refuse",
  "necessary",
  "essential",
  "required",
  "only",
  "selected",
  "selection",
  "settings",
  "preferences",
  "manage",
  "customize",
  "customise",
  "options",
  "choices",
  "learn more",
  "more info",
  "read more",
  "details",
  "policy",
  "without accepting",
  "do not",
  "don't",
  "no thanks",
  "not now",
  "later",
  "opt out",
  "opt-out",
  "disagree",
  "cancel",
  "save",
  "confirm",
  // vi
  "từ chối",
  "tùy chỉnh",
  "tuỳ chỉnh",
  "quản lý",
  "cài đặt",
  "thiết lập",
  "tùy chọn",
  "cần thiết",
  "lưu",
  "tìm hiểu thêm",
  "xem thêm",
  "chính sách",
  // de
  "ablehnen",
  "notwendige",
  "notwendigen",
  "erforderliche",
  "nur",
  "einstellungen",
  "verwalten",
  "anpassen",
  "auswahl",
  "speichern",
  "mehr erfahren",
  "weitere",
  "datenschutz",
  "widersprechen",
  "nicht",
  // fr
  "refuser",
  "continuer sans",
  "sans accepter",
  "paramétrer",
  "paramètres",
  "personnaliser",
  "gérer",
  "choix",
  "enregistrer",
  "en savoir plus",
  "nécessaires",
  "essentiels",
  "uniquement",
  "seulement",
  "non",
  // es
  "rechazar",
  "denegar",
  "configurar",
  "configuración",
  "gestionar",
  "administrar",
  "personalizar",
  "preferencias",
  "solo",
  "sólo",
  "guardar",
  "más información",
  "saber más",
  "sin aceptar",
  "no",
  // pt
  "rejeitar",
  "recusar",
  "gerir",
  "gerenciar",
  "configurações",
  "preferências",
  "apenas",
  "somente",
  "salvar",
  "saiba mais",
  "mais informações",
  "não",
  // it
  "rifiuta",
  "rifiuto",
  "gestisci",
  "personalizza",
  "impostazioni",
  "preferenze",
  "salva",
  "scopri di più",
  "maggiori informazioni",
  // ja
  "拒否",
  "管理",
  "設定",
  "カスタマイズ",
  "必要なもののみ",
  "必須のみ",
  "のみ",
  "保存",
  "詳細",
  "詳しく",
  "同意しない",
  "許可しない",
  // ko
  "거부",
  "관리",
  "설정",
  "필수",
  "맞춤",
  "저장",
  "자세히",
  "동의하지",
  "거절",
  // zh
  "拒绝",
  "拒絕",
  "管理",
  "设置",
  "設定",
  "仅",
  "僅",
  "必要",
  "自定义",
  "自訂",
  "偏好",
  "保存",
  "了解更多",
  "不同意",
  // ru
  "отклонить",
  "отказаться",
  "отказ",
  "настроить",
  "настройки",
  "управление",
  "управлять",
  "только",
  "необходимые",
  "обязательные",
  "сохранить",
  "подробнее",
  // tr
  "reddet",
  "kabul etme",
  "ayarlar",
  "yönet",
  "özelleştir",
  "tercihler",
  "yalnızca",
  "sadece",
  "gerekli",
  "kaydet",
  "daha fazla",
  "ayrıntılar",
  "hayır",
];

/// Wording that marks a banner as being about cookies or privacy, matched as a
/// substring of the banner's text and its ids and class names.
const BANNER_WORDS: &[&str] = &[
  "cookie",
  "consent",
  "gdpr",
  "ccpa",
  "privacy",
  "tracking",
  "personal data",
  "onetrust",
  "didomi",
  "usercentrics",
  "cookiebot",
  "trustarc",
  "truste",
  "quantcast",
  "sourcepoint",
  "osano",
  "iubenda",
  "termly",
  "klaro",
  "borlabs",
  "complianz",
  "cookieyes",
  "cky-",
  "cc-window",
  "cmp-",
  "-cmp",
  "evidon",
  // vi
  "quyền riêng tư",
  "bảo mật",
  "dữ liệu cá nhân",
  // de
  "datenschutz",
  "einwilligung",
  // fr
  "confidentialité",
  "témoins",
  "consentement",
  // es
  "privacidad",
  "consentimiento",
  "galletas",
  // pt
  "privacidade",
  "consentimento",
  // it
  "informativa",
  "consenso",
  // ja
  "クッキー",
  "プライバシー",
  "個人情報",
  // ko
  "쿠키",
  "개인정보",
  "개인 정보",
  // zh
  "隐私",
  "隱私",
  // ru
  "куки",
  "конфиденциальност",
  "персональн",
  // tr
  "çerez",
  "gizlilik",
  "kvkk",
];

/// A button the page read found, with the banner it lives in.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ButtonCandidate {
  /// Its accessible name: aria-label, text, value or title.
  pub name: String,
  pub rect: Rect,
  /// The nearest banner-like ancestor (a dialog, or a fixed or sticky box)
  /// exists.
  pub in_banner: bool,
  /// The banner's text and the ids and class names on the way up to it.
  #[serde(default)]
  pub context: String,
}

/// How firmly `label` accepts a banner: 2 for an accept-all phrase, 1 for a
/// bare "accept" or "OK", `None` for anything else.
pub fn accept_strength(label: &str) -> Option<u8> {
  let text = normalize(label);
  if text.is_empty() || text.chars().count() > 70 {
    return None;
  }
  if has_any(&text, NOT_ACCEPT) || forbidden_action(label) {
    return None;
  }
  if has_any(&text, ACCEPT_PHRASES) {
    return Some(2);
  }
  is_any(&text, ACCEPT_EXACT).then_some(1)
}

/// Whether the banner's wording is about cookies or privacy.
pub fn is_consent_context(context: &str) -> bool {
  let lowered = context.to_lowercase();
  BANNER_WORDS.iter().any(|word| lowered.contains(word))
}

/// The index of the button to press to accept, if the page shows a banner with
/// one. The firmest label wins; among equals the larger button (the one the
/// banner makes prominent).
pub fn pick_accept(buttons: &[ButtonCandidate]) -> Option<usize> {
  buttons
    .iter()
    .enumerate()
    .filter(|(_, button)| button.in_banner && is_consent_context(&button.context))
    .filter(|(_, button)| button.rect.w >= 18.0 && button.rect.h >= 12.0)
    .filter_map(|(index, button)| {
      accept_strength(&button.name).map(|strength| (index, strength, button.rect.area()))
    })
    .max_by(|a, b| a.1.cmp(&b.1).then(a.2.total_cmp(&b.2)))
    .map(|(index, _, _)| index)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn button(name: &str, context: &str) -> ButtonCandidate {
    ButtonCandidate {
      name: name.to_string(),
      rect: Rect {
        x: 100.0,
        y: 600.0,
        w: 140.0,
        h: 40.0,
      },
      in_banner: true,
      context: context.to_string(),
    }
  }

  #[test]
  fn accept_labels_are_recognised_in_every_supported_language() {
    let accepting = [
      "Accept all cookies",
      "Accept",
      "I agree",
      "Got it",
      "OK",
      "Allow all",
      "Chấp nhận tất cả",
      "Đồng ý",
      "Alle akzeptieren",
      "Zustimmen",
      "Tout accepter",
      "J'accepte",
      "Accepter",
      "Aceptar todo",
      "Aceptar",
      "Aceitar tudo",
      "Aceitar",
      "Accetta tutto",
      "Accetta",
      "すべて同意",
      "同意する",
      "同意",
      "모두 수락",
      "동의합니다",
      "수락",
      "全部接受",
      "接受",
      "我同意",
      "Принять все",
      "Принять",
      "Согласен",
      "Tümünü kabul et",
      "Kabul et",
      "Kabul ediyorum",
    ];
    for label in accepting {
      assert!(accept_strength(label).is_some(), "{label} should accept");
    }
    assert_eq!(accept_strength("Accept all cookies and continue"), Some(2));
    assert_eq!(accept_strength("Accept"), Some(1));
    assert_eq!(accept_strength("  \u{2713} Accept all \u{203A}"), Some(2));
  }

  #[test]
  fn refusals_settings_and_narrower_accepts_are_never_accepts() {
    let not_accepting = [
      "Reject all",
      "Decline",
      "Accept only necessary cookies",
      "Accept selected",
      "Cookie settings",
      "Manage preferences",
      "Customize",
      "Learn more",
      "Save my choices",
      "Do not accept",
      "Continue without accepting",
      "Disagree",
      "Từ chối",
      "Tùy chỉnh",
      "Chỉ cookie cần thiết",
      "Ablehnen",
      "Nur notwendige akzeptieren",
      "Einstellungen",
      "Tout refuser",
      "Continuer sans accepter",
      "Personnaliser",
      "Rechazar todo",
      "Configurar cookies",
      "Solo necesarias",
      "Rejeitar tudo",
      "Aceitar apenas necessários",
      "Gerir preferências",
      "Rifiuta tutto",
      "Solo necessari",
      "Impostazioni",
      "すべて拒否",
      "必要なもののみ許可",
      "同意しない",
      "設定",
      "거부",
      "필수만 허용",
      "동의하지 않음",
      "拒绝全部",
      "仅接受必要",
      "不同意",
      "Отклонить",
      "Только необходимые",
      "Настроить",
      "Reddet",
      "Yalnızca gerekli olanları kabul et",
      "Ayarlar",
      "",
      "Subscribe",
      "Buy now",
    ];
    for label in not_accepting {
      assert_eq!(accept_strength(label), None, "{label} must not accept");
    }
  }

  #[test]
  fn an_accept_word_next_to_an_account_action_is_not_pressed() {
    for label in [
      "Accept and sign in",
      "Agree and subscribe",
      "Accept and buy",
      "I agree, log in",
    ] {
      assert_eq!(accept_strength(label), None, "{label}");
    }
  }

  #[test]
  fn a_banner_is_known_by_its_wording_or_its_ids() {
    assert!(is_consent_context(
      "We use cookies to improve your experience"
    ));
    assert!(is_consent_context(
      "div#onetrust-banner-sdk onetrust-banner"
    ));
    assert!(is_consent_context("Chúng tôi sử dụng cookie"));
    assert!(is_consent_context("Wir respektieren Ihren Datenschutz"));
    assert!(is_consent_context("このサイトはクッキーを使用しています"));
    assert!(is_consent_context("本网站使用 Cookie 保护隐私"));
    assert!(is_consent_context("Мы используем файлы cookie"));
    assert!(is_consent_context("Bu site çerez kullanır"));
    assert!(!is_consent_context("Subscribe to our newsletter"));
    assert!(!is_consent_context(""));
  }

  #[test]
  fn the_button_to_press_is_picked_from_the_banner_and_nowhere_else() {
    let context = "We use cookies. div#cookie-banner";
    let buttons = vec![
      button("Manage preferences", context),
      button("Reject all", context),
      button("Accept all", context),
    ];
    assert_eq!(pick_accept(&buttons), Some(2));

    // The same label outside a banner is a page button, not an answer to one.
    let mut outside = vec![button("Accept all", context)];
    outside[0].in_banner = false;
    assert_eq!(pick_accept(&outside), None);

    // A banner about something else is not a consent banner.
    assert_eq!(
      pick_accept(&[button("Accept", "Join our newsletter dialog")]),
      None
    );

    // A firmer label wins over a bare one; a tiny button is no target.
    let mut small = button("Accept all", context);
    small.rect.w = 6.0;
    let both = vec![button("OK", context), small, button("Accept all", context)];
    assert_eq!(pick_accept(&both), Some(2));
    assert_eq!(pick_accept(&[]), None);
  }
}
