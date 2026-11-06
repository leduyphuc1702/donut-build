//! Choosing what to click, from what a page looks like and not from a script.
//!
//! There is no selector or path for a site here. The page read reports the
//! links a person could see right now (their box, their text, which part of the
//! page they sit in), and this module decides which are fair: links to another
//! page of the same site, in the visible part of the window, that do not log
//! in, buy, delete, download or leave. Of those it prefers the ones a reader
//! would: headlines and body links over the footer's, text over bare icons.
//! The pick is random, weighted by that preference.

use super::pointer::Rect;
use super::rhythm::Draw;
use super::text::{has_any, is_any, normalize};
use serde::Deserialize;
use std::collections::HashSet;
use url::Url;

/// Wording that a link or button must not carry: signing in or out, buying,
/// paying, deleting, downloading, giving money, accounts, ads. Matched on the
/// visible text, the accessible name, and nothing else.
const FORBIDDEN_ACTIONS: &[&str] = &[
  // en
  "log in",
  "login",
  "sign in",
  "signin",
  "sign up",
  "signup",
  "log out",
  "logout",
  "sign out",
  "register",
  "subscribe",
  "unsubscribe",
  "buy",
  "purchase",
  "checkout",
  "check out",
  "add to cart",
  "add to bag",
  "add to basket",
  "cart",
  "basket",
  "order",
  "pay",
  "payment",
  "delete",
  "remove",
  "donate",
  "download",
  "account",
  "password",
  "settings",
  "sponsored",
  "advertisement",
  "advertise",
  "promoted",
  "adchoices",
  "install",
  "upgrade",
  "free trial",
  "get started",
  "join now",
  "reply",
  "submit",
  "book now",
  "reserve",
  "print",
  "share",
  // vi
  "đăng nhập",
  "đăng ký",
  "đăng xuất",
  "mua",
  "mua ngay",
  "thanh toán",
  "giỏ hàng",
  "đặt hàng",
  "xóa",
  "xoá",
  "tải xuống",
  "tải về",
  "quảng cáo",
  "tài khoản",
  "mật khẩu",
  "nạp tiền",
  "ủng hộ",
  // de
  "anmelden",
  "einloggen",
  "registrieren",
  "abonnieren",
  "kaufen",
  "bestellen",
  "warenkorb",
  "kasse",
  "löschen",
  "abmelden",
  "ausloggen",
  "herunterladen",
  "spenden",
  "konto",
  "passwort",
  "werbung",
  "anzeige",
  "bezahlen",
  // fr
  "se connecter",
  "connexion",
  "s'inscrire",
  "inscription",
  "acheter",
  "panier",
  "commander",
  "paiement",
  "payer",
  "supprimer",
  "déconnexion",
  "se déconnecter",
  "s'abonner",
  "télécharger",
  "compte",
  "mot de passe",
  "publicité",
  "faire un don",
  // es
  "iniciar sesión",
  "acceder",
  "registrarse",
  "regístrate",
  "suscribirse",
  "suscríbete",
  "comprar",
  "carrito",
  "cesta",
  "pagar",
  "pago",
  "eliminar",
  "borrar",
  "cerrar sesión",
  "descargar",
  "donar",
  "cuenta",
  "contraseña",
  "publicidad",
  "anuncio",
  // pt
  "entrar",
  "cadastrar",
  "cadastre-se",
  "assinar",
  "carrinho",
  "pagamento",
  "excluir",
  "apagar",
  "sair",
  "baixar",
  "doar",
  "conta",
  "senha",
  "publicidade",
  "anúncio",
  "finalizar compra",
  "criar conta",
  // it
  "accedi",
  "registrati",
  "iscriviti",
  "acquista",
  "compra",
  "carrello",
  "paga",
  "elimina",
  "cancella",
  "esci",
  "scarica",
  "dona",
  "pubblicità",
  "annuncio",
  // ja
  "ログイン",
  "サインイン",
  "新規登録",
  "会員登録",
  "登録",
  "購読",
  "購入",
  "カート",
  "削除",
  "ログアウト",
  "注文",
  "支払",
  "ダウンロード",
  "寄付",
  "アカウント",
  "パスワード",
  "広告",
  // ko
  "로그인",
  "회원가입",
  "가입",
  "구독",
  "구매",
  "장바구니",
  "삭제",
  "로그아웃",
  "결제",
  "주문",
  "다운로드",
  "기부",
  "계정",
  "비밀번호",
  "광고",
  // zh
  "登录",
  "登錄",
  "登入",
  "注册",
  "註冊",
  "订阅",
  "訂閱",
  "购买",
  "購買",
  "购物车",
  "購物車",
  "删除",
  "刪除",
  "退出",
  "结算",
  "結算",
  "支付",
  "下单",
  "下單",
  "下载",
  "下載",
  "捐赠",
  "捐款",
  "账号",
  "帳號",
  "账户",
  "密码",
  "密碼",
  "广告",
  "廣告",
  // ru
  "войти",
  "вход",
  "регистрация",
  "зарегистрироваться",
  "подписаться",
  "купить",
  "корзина",
  "оформить",
  "оплатить",
  "оплата",
  "удалить",
  "выйти",
  "выход",
  "скачать",
  "пожертвовать",
  "аккаунт",
  "личный кабинет",
  "пароль",
  "реклама",
  "заказать",
  // tr
  "giriş",
  "oturum aç",
  "kayıt ol",
  "üye ol",
  "abone ol",
  "satın al",
  "sepet",
  "ödeme",
  "öde",
  "sil",
  "çıkış",
  "indir",
  "bağış",
  "hesap",
  "şifre",
  "reklam",
  "sipariş",
];

/// Whether `text` (a link's words or a button's name) names an action the
/// warm-up never takes.
pub fn forbidden_action(text: &str) -> bool {
  let normal = normalize(text);
  has_any(&normal, FORBIDDEN_ACTIONS) || is_any(&normal, &["ad", "ads", "ad choices"])
}

/// Path pieces (split at `/ - _ .`) that lead to an account, a basket, a
/// payment, a state change, a download or a redirect.
const DENIED_PATH_TOKENS: &[&str] = &[
  "login",
  "signin",
  "signup",
  "register",
  "registration",
  "logout",
  "signout",
  "cart",
  "basket",
  "checkout",
  "payment",
  "payments",
  "billing",
  "subscribe",
  "unsubscribe",
  "delete",
  "remove",
  "account",
  "accounts",
  "settings",
  "admin",
  "oauth",
  "oauth2",
  "sso",
  "password",
  "donate",
  "redirect",
  "affiliate",
  "share",
  "print",
  "feed",
  "rss",
  "download",
  "downloads",
  "track",
  "tracking",
  "ads",
  "adserver",
  "xmlrpc",
  "api",
  "order",
  "orders",
  "wishlist",
];

/// Phrases of a path that split apart on `-` and still mean one thing.
const DENIED_PATH_PHRASES: &[&str] = &[
  "log-in",
  "sign-in",
  "sign-up",
  "log-out",
  "sign-out",
  "cdn-cgi",
  "wp-admin",
  "wp-login",
  "my-account",
];

/// Parts of a query that change something or leave.
const DENIED_QUERY: &[&str] = &[
  "logout",
  "signout",
  "add-to-cart",
  "add_to_cart",
  "addtocart",
  "remove_item",
  "delete",
  "unsubscribe",
  "subscribe",
  "download",
];

/// First labels of a host that are an account or payment area.
const DENIED_HOST_LABELS: &[&str] = &[
  "login", "accounts", "account", "auth", "sso", "pay", "payment", "payments", "checkout",
  "billing", "admin",
];

/// Files that are not pages to read.
const DENIED_EXTENSIONS: &[&str] = &[
  "pdf", "zip", "rar", "7z", "gz", "tar", "tgz", "dmg", "pkg", "exe", "msi", "deb", "rpm", "apk",
  "ipa", "iso", "bin", "mp3", "mp4", "m4a", "avi", "mov", "mkv", "wav", "flac", "doc", "docx",
  "xls", "xlsx", "ppt", "pptx", "csv", "json", "xml", "rss", "atom", "jpg", "jpeg", "png", "gif",
  "webp", "svg", "ico",
];

/// Second-level labels under which a registered name takes one more label.
const SECOND_LEVELS: &[&str] = &[
  "co.uk", "org.uk", "ac.uk", "gov.uk", "me.uk", "com.au", "net.au", "org.au", "edu.au", "gov.au",
  "co.nz", "org.nz", "co.jp", "ne.jp", "or.jp", "ac.jp", "co.kr", "or.kr", "com.cn", "net.cn",
  "org.cn", "gov.cn", "com.hk", "com.tw", "com.sg", "com.my", "com.vn", "net.vn", "org.vn",
  "edu.vn", "gov.vn", "com.br", "net.br", "org.br", "com.ar", "com.mx", "com.co", "com.tr",
  "com.ua", "co.in", "co.id", "co.th", "co.za", "com.ph", "com.pk", "com.bd", "com.ng", "com.eg",
  "com.sa", "co.il", "com.pe", "com.ve", "com.ec", "com.uy",
];

/// Hosts where every customer has a subdomain of their own: two of them are not
/// one site.
const SHARED_HOSTS: &[&str] = &[
  "github.io",
  "gitlab.io",
  "pages.dev",
  "vercel.app",
  "netlify.app",
  "herokuapp.com",
  "blogspot.com",
  "wordpress.com",
  "azurewebsites.net",
  "web.app",
  "firebaseapp.com",
  "appspot.com",
  "onrender.com",
  "fly.dev",
  "workers.dev",
  "amazonaws.com",
  "cloudfront.net",
];

/// The registered name of a host: `shop.example.co.uk` is `example.co.uk`. An
/// address or a bare name is itself. There is no public-suffix list here, so a
/// short table stands in for it.
pub fn registrable_domain(host: &str) -> String {
  let host = host.trim().trim_end_matches('.').to_lowercase();
  let labels: Vec<&str> = host.split('.').collect();
  let numeric = labels.iter().all(|label| label.parse::<u8>().is_ok());
  if labels.len() <= 2 || numeric || host.contains(':') {
    return host.strip_prefix("www.").unwrap_or(&host).to_string();
  }
  let last_two = labels[labels.len() - 2..].join(".");
  let keep =
    if SHARED_HOSTS.contains(&last_two.as_str()) || SECOND_LEVELS.contains(&last_two.as_str()) {
      3
    } else {
      2
    };
  labels[labels.len().saturating_sub(keep)..].join(".")
}

/// A page's identity for "have I been here": no fragment, no `www.`, one
/// scheme, no trailing slash, tracking parameters dropped.
pub fn canonical_url(url: &Url) -> String {
  let host = url.host_str().unwrap_or_default().to_lowercase();
  let host = host.strip_prefix("www.").unwrap_or(&host);
  let path = url.path().trim_end_matches('/');
  let mut canonical = format!("{host}{path}");
  let kept: Vec<String> = url
    .query_pairs()
    .filter(|(key, _)| !key.starts_with("utm_") && key != "fbclid" && key != "gclid")
    .map(|(key, value)| format!("{key}={value}"))
    .collect();
  if !kept.is_empty() {
    canonical.push('?');
    canonical.push_str(&kept.join("&"));
  }
  canonical
}

/// Whether a link to `url` is one the warm-up may follow from a page of `site`
/// (a registrable domain).
pub fn url_allowed(url: &Url, site: &str) -> bool {
  if !matches!(url.scheme(), "http" | "https") {
    return false;
  }
  let Some(host) = url.host_str() else {
    return false;
  };
  if registrable_domain(host) != site {
    return false;
  }
  let first = host.split('.').next().unwrap_or_default();
  if DENIED_HOST_LABELS.contains(&first) && host.matches('.').count() >= 2 {
    return false;
  }
  let path = url.path().to_lowercase();
  if let Some(file) = path.rsplit('/').next() {
    if let Some((_, extension)) = file.rsplit_once('.') {
      if DENIED_EXTENSIONS.contains(&extension) {
        return false;
      }
    }
  }
  if DENIED_PATH_PHRASES
    .iter()
    .any(|phrase| path.contains(phrase))
  {
    return false;
  }
  if path
    .split(['/', '-', '_', '.'])
    .any(|token| DENIED_PATH_TOKENS.contains(&token))
  {
    return false;
  }
  let query = url.query().unwrap_or_default().to_lowercase();
  !DENIED_QUERY.iter().any(|part| query.contains(part))
}

/// Which part of the page a link sits in.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Region {
  Content,
  Nav,
  Header,
  Footer,
  Aside,
  Other,
}

/// A link the page read found, visible and not covered.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkCandidate {
  pub href: String,
  #[serde(default)]
  pub text: String,
  /// The part of the link inside the window.
  pub rect: Rect,
  pub region: Region,
  /// It asks for a new tab (`target="_blank"`).
  #[serde(default)]
  pub new_tab: bool,
  /// It is a download.
  #[serde(default)]
  pub download: bool,
  #[serde(default)]
  pub rel: String,
}

/// What the choice depends on besides the link itself.
pub struct Context<'a> {
  /// The registrable domain the run started on.
  pub site: &'a str,
  /// The page being read, canonical.
  pub current: Option<&'a str>,
  /// Pages already visited on this site, canonical.
  pub visited: &'a HashSet<String>,
  /// The window, in CSS pixels.
  pub viewport: (f64, f64),
}

/// How much a reader would want this link, or `None` when it is not fair game.
pub fn score_link(link: &LinkCandidate, ctx: &Context) -> Option<f64> {
  let url = Url::parse(&link.href).ok()?;
  if link.download || !url_allowed(&url, ctx.site) {
    return None;
  }
  let canonical = canonical_url(&url);
  if ctx.visited.contains(&canonical) || ctx.current == Some(canonical.as_str()) {
    return None;
  }
  let rel = link.rel.to_lowercase();
  if ["nofollow", "sponsored", "ugc"]
    .iter()
    .any(|word| rel.contains(word))
  {
    return None;
  }
  if forbidden_action(&link.text) {
    return None;
  }
  let (width, height) = (link.rect.w, link.rect.h);
  let (view_w, view_h) = ctx.viewport;
  if width < 14.0 || height < 8.0 || link.rect.area() > view_w * view_h * 0.6 {
    return None;
  }

  let region = match link.region {
    Region::Content => 3.0,
    Region::Other => 1.5,
    Region::Nav => 1.0,
    Region::Header => 0.8,
    Region::Aside => 0.5,
    Region::Footer => 0.15,
  };
  let length = link.text.trim().chars().count();
  let text = match length {
    0 => 0.5,
    1..=3 => 0.4,
    4..=11 => 1.0,
    12..=70 => 1.4,
    71..=140 => 1.0,
    _ => 0.5,
  };
  let size = (1.0 + link.rect.area() / 600.0).ln().clamp(0.5, 3.0);
  let center = link.rect.center();
  // People look at the upper middle of the window first.
  let place = (1.2 - 0.6 * (center.y / view_h.max(1.0) - 0.45).abs()).clamp(0.5, 1.2);
  let tab = if link.new_tab { 0.4 } else { 1.0 };
  Some(region * text * size * place * tab)
}

/// The index of a draw weighted by `scores`; `None` entries are never drawn.
pub fn pick_weighted(scores: &[Option<f64>], d: &mut impl Draw) -> Option<usize> {
  let total: f64 = scores.iter().flatten().sum();
  if total <= 0.0 {
    return None;
  }
  let mut ticket = d.unit() * total;
  let mut last = None;
  for (index, score) in scores.iter().enumerate() {
    let Some(score) = score.filter(|score| *score > 0.0) else {
      continue;
    };
    last = Some(index);
    if ticket < score {
      return Some(index);
    }
    ticket -= score;
  }
  last
}

/// The link to follow, if the window holds a fair one.
pub fn choose_link(links: &[LinkCandidate], ctx: &Context, d: &mut impl Draw) -> Option<usize> {
  let scores: Vec<Option<f64>> = links.iter().map(|link| score_link(link, ctx)).collect();
  pick_weighted(&scores, d)
}

#[cfg(test)]
mod tests {
  use super::super::rhythm::Seeded;
  use super::*;

  fn same_site(a: &str, b: &str) -> bool {
    registrable_domain(a) == registrable_domain(b)
  }

  fn link(href: &str, text: &str, region: Region) -> LinkCandidate {
    LinkCandidate {
      href: href.to_string(),
      text: text.to_string(),
      rect: Rect {
        x: 200.0,
        y: 300.0,
        w: 220.0,
        h: 28.0,
      },
      region,
      new_tab: false,
      download: false,
      rel: String::new(),
    }
  }

  fn score(candidate: &LinkCandidate, visited: &HashSet<String>) -> Option<f64> {
    score_link(
      candidate,
      &Context {
        site: "example.com",
        current: Some("example.com"),
        visited,
        viewport: (1280.0, 700.0),
      },
    )
  }

  #[test]
  fn a_registered_name_is_found_with_and_without_a_two_part_suffix() {
    assert_eq!(registrable_domain("www.example.com"), "example.com");
    assert_eq!(registrable_domain("shop.eu.example.com"), "example.com");
    assert_eq!(registrable_domain("example.com"), "example.com");
    assert_eq!(registrable_domain("shop.example.co.uk"), "example.co.uk");
    assert_eq!(registrable_domain("tuoitre.com.vn"), "tuoitre.com.vn");
    assert_eq!(
      registrable_domain("m.news.tuoitre.com.vn"),
      "tuoitre.com.vn"
    );
    assert_eq!(registrable_domain("alice.github.io"), "alice.github.io");
    assert_eq!(registrable_domain("a.b.alice.github.io"), "alice.github.io");
    assert_eq!(registrable_domain("127.0.0.1"), "127.0.0.1");
    assert_eq!(registrable_domain("localhost"), "localhost");
    assert_eq!(registrable_domain("Example.COM."), "example.com");
    assert!(same_site("blog.example.com", "www.example.com"));
    assert!(!same_site("example.com", "example.org"));
    assert!(!same_site("alice.github.io", "bob.github.io"));
  }

  #[test]
  fn canonical_pages_ignore_fragments_www_slashes_and_tracking() {
    let a = Url::parse("https://www.example.com/news/story/?utm_source=x#top").unwrap();
    let b = Url::parse("http://example.com/news/story").unwrap();
    assert_eq!(canonical_url(&a), canonical_url(&b));
    let c = Url::parse("https://example.com/news/story?id=3").unwrap();
    assert_ne!(canonical_url(&b), canonical_url(&c));
  }

  #[test]
  fn only_ordinary_pages_of_the_same_site_are_allowed() {
    let allowed = |raw: &str| url_allowed(&Url::parse(raw).unwrap(), "example.com");
    assert!(allowed("https://example.com/news/story-of-the-day"));
    assert!(allowed("https://blog.example.com/2026/05/hello"));
    for refused in [
      "https://other.org/news",
      "mailto:a@example.com",
      "tel:+1555",
      "javascript:void(0)",
      "ftp://example.com/x",
      "https://example.com/login",
      "https://example.com/users/sign-in",
      "https://example.com/Cart",
      "https://example.com/checkout/step-1",
      "https://example.com/account/orders",
      "https://example.com/logout?next=/",
      "https://example.com/shop/add-to-cart-now",
      "https://example.com/?add-to-cart=12",
      "https://example.com/files/report.pdf",
      "https://example.com/download/app.zip",
      "https://example.com/wp-login.php",
      "https://example.com/cdn-cgi/l/email-protection",
      "https://accounts.example.com/home",
      "https://example.com/feed",
      "https://example.com/products/photo.jpg",
    ] {
      assert!(!allowed(refused), "{refused} must be refused");
    }
    // A word that merely contains a denied one is fine.
    assert!(allowed("https://example.com/ordering-a-pizza-guide"));
    assert!(allowed("https://example.com/cartography"));
  }

  #[test]
  fn account_and_purchase_words_are_refused_in_every_language() {
    for text in [
      "Log in",
      "Sign up for free",
      "Add to cart",
      "Buy now",
      "Checkout",
      "Delete my data",
      "Download the app",
      "Đăng nhập",
      "Giỏ hàng",
      "Mua ngay",
      "Anmelden",
      "Warenkorb",
      "Se connecter",
      "Mon compte",
      "Iniciar sesión",
      "Añadir al carrito",
      "Comprar ahora",
      "Entrar",
      "Finalizar compra",
      "Accedi",
      "Carrello",
      "ログイン",
      "カートに追加",
      "로그인",
      "장바구니",
      "登录",
      "加入购物车",
      "Войти",
      "Корзина",
      "Oturum aç",
      "Sepet",
      "Sponsored",
      "Ad",
    ] {
      assert!(forbidden_action(text), "{text} must be refused");
    }
    for text in [
      "Read the full story",
      "Latest news",
      "About us",
      "Tin tức mới nhất",
      "Über uns",
      "Notre histoire",
      "Nuestro equipo",
      "最新ニュース",
      "회사 소개",
      "关于我们",
      "О компании",
      "Hakkımızda",
    ] {
      assert!(!forbidden_action(text), "{text} is fair");
    }
  }

  #[test]
  fn a_fair_link_scores_and_an_unfair_one_does_not() {
    let none = HashSet::new();
    let fair = link(
      "https://example.com/news/story",
      "Why the market moved today",
      Region::Content,
    );
    assert!(score(&fair, &none).is_some_and(|score| score > 0.0));

    let mut external = fair.clone();
    external.href = "https://elsewhere.org/story".into();
    assert_eq!(score(&external, &none), None);
    assert_eq!(
      score(
        &link("https://example.com/login", "Continue", Region::Content),
        &none
      ),
      None
    );
    assert_eq!(
      score(
        &link("https://example.com/a", "Sign in", Region::Content),
        &none
      ),
      None
    );
    assert_eq!(
      score(&link("not a url", "Story", Region::Content), &none),
      None
    );

    let mut download = fair.clone();
    download.download = true;
    assert_eq!(score(&download, &none), None);
    let mut sponsored = fair.clone();
    sponsored.rel = "nofollow sponsored".into();
    assert_eq!(score(&sponsored, &none), None);
    let mut tiny = fair.clone();
    tiny.rect.w = 6.0;
    assert_eq!(score(&tiny, &none), None);
    let mut huge = fair.clone();
    huge.rect = Rect {
      x: 0.0,
      y: 0.0,
      w: 1280.0,
      h: 700.0,
    };
    assert_eq!(score(&huge, &none), None);
  }

  #[test]
  fn a_page_already_seen_or_the_current_one_is_not_chosen_again() {
    let mut visited = HashSet::new();
    let seen = link(
      "https://example.com/news/story#comments",
      "Why the market moved today",
      Region::Content,
    );
    assert!(score(&seen, &visited).is_some());
    visited.insert("example.com/news/story".to_string());
    assert_eq!(score(&seen, &visited), None);

    let here = link("https://www.example.com/#top", "Home", Region::Nav);
    assert_eq!(score(&here, &HashSet::new()), None, "the current page");
  }

  #[test]
  fn readers_links_outweigh_the_footers_and_text_outweighs_a_bare_icon() {
    let none = HashSet::new();
    let body = score(
      &link(
        "https://example.com/a",
        "A long and interesting headline",
        Region::Content,
      ),
      &none,
    )
    .unwrap();
    let footer = score(
      &link(
        "https://example.com/b",
        "A long and interesting headline",
        Region::Footer,
      ),
      &none,
    )
    .unwrap();
    let icon = score(&link("https://example.com/c", "", Region::Content), &none).unwrap();
    let tab = {
      let mut opens = link(
        "https://example.com/d",
        "A long and interesting headline",
        Region::Content,
      );
      opens.new_tab = true;
      score(&opens, &none).unwrap()
    };
    assert!(body > footer * 10.0);
    assert!(body > icon * 2.0);
    assert!(body > tab * 2.0);
  }

  #[test]
  fn the_draw_follows_the_weights_and_skips_what_is_unfair() {
    let scores = [Some(8.0), None, Some(2.0), Some(0.0)];
    let mut counts = [0usize; 4];
    let mut d = Seeded::new(1);
    for _ in 0..20_000 {
      counts[pick_weighted(&scores, &mut d).unwrap()] += 1;
    }
    assert_eq!(counts[1], 0);
    assert_eq!(counts[3], 0);
    let ratio = counts[0] as f64 / counts[2] as f64;
    assert!((3.4..4.8).contains(&ratio), "{ratio}");
    assert_eq!(pick_weighted(&[None, None], &mut d), None);
    assert_eq!(pick_weighted(&[], &mut d), None);
  }

  #[test]
  fn choosing_among_links_picks_a_fair_one_or_nothing() {
    let none = HashSet::new();
    let ctx = Context {
      site: "example.com",
      current: None,
      visited: &none,
      viewport: (1280.0, 700.0),
    };
    let links = vec![
      link("https://example.com/login", "Log in", Region::Nav),
      link("https://other.org/x", "Elsewhere", Region::Content),
      link(
        "https://example.com/story",
        "The story of the week",
        Region::Content,
      ),
    ];
    let mut d = Seeded::new(4);
    for _ in 0..200 {
      assert_eq!(choose_link(&links, &ctx, &mut d), Some(2));
    }
    assert_eq!(choose_link(&links[..2], &ctx, &mut d), None);
  }
}
