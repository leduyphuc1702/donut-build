//! The Windows fonts a Windows persona needs on a macOS or Linux machine.
//!
//! Apostate lists a persona's fonts from the host's own: a Windows persona on
//! a Mac shows only the Windows families the Mac has, and without Segoe UI
//! Variable and Sitka it claims Windows 11 with Windows 10's fonts, which
//! fingerprinting pages flag. Donut installs the Windows 11 core families by
//! itself, in the background after the engine, into the directory
//! `apostate fonts install windows` uses, so an earlier manual install counts.
//! A launch that comes first waits for the same download. A Windows machine
//! has them already; a macOS persona's fonts cannot be downloaded, so a macOS
//! persona off a Mac still shows the host's.

use crate::log_streak::Streak;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use tokio::sync::Mutex as AsyncMutex;

/// A pinned commit of `MauCariApa-com/windows-11-fonts` (MIT), the source
/// `apostate fonts install windows` clones.
const FONTS_BASE: &str = "https://raw.githubusercontent.com/MauCariApa-com/windows-11-fonts/96cc06871ca9d92093f8fa46242025dd2c4ce981/w11-fonts";
/// The repository has no Marlett; it is read out of this archive with byte
/// ranges, about 100 KB of a 190 MB file.
const MARLETT_ZIP: &str =
  "https://github.com/liblaf/fonts/releases/download/Win11/Win11-English.zip";
const ZIP_TAIL: u64 = 65_536;
const PARALLEL_DOWNLOADS: usize = 4;

struct FontFile {
  name: &'static str,
  size: u64,
  sha256: &'static str,
}

const MARLETT: FontFile = FontFile {
  name: "marlett.ttf",
  size: 27_724,
  sha256: "b7397adf2dcc24ca790348a3c26deb2122b45e5728fd25fc588de4cf5a75b469",
};

/// Every file of the repository whose family (name ID 1) a Windows core font
/// pack of Apostate's `font_packs.json` lists or requires. The engine's own
/// installer reads only the lists, which name Segoe UI Variable's instances
/// (Display, Text, Small) but not the family SegUIVar.ttf answers to, so it
/// leaves out the file a Windows 11 persona is told apart by.
const WINDOWS_FONTS: &[FontFile] = &[
  FontFile {
    name: "Candara.ttf",
    size: 219532,
    sha256: "9f02f6bf076197ed2a1335fe59a3bdf88e7ad4a849bf68037a528f37cce36791",
  },
  FontFile {
    name: "Candarab.ttf",
    size: 227604,
    sha256: "5346840fa85a1969699dc97635e838cd2c42dad7f8a4999d7368eb888fceb04b",
  },
  FontFile {
    name: "Candarai.ttf",
    size: 227648,
    sha256: "fc68cb42582e1207bcc397df7b60870a74ae068b17d7bd79f2e9f63af89796b4",
  },
  FontFile {
    name: "Candaraz.ttf",
    size: 229364,
    sha256: "1614c0d3a6751eb46b2c52f09ae23dd61f769beda83058269cb0653c049eae43",
  },
  FontFile {
    name: "Gabriola.ttf",
    size: 1806004,
    sha256: "ddd852f9e43a5e81af62a3f6654324ba7553d7d66101ab848abc1bd2d626ac4b",
  },
  FontFile {
    name: "Inkfree.ttf",
    size: 198972,
    sha256: "c8ab96bc49ec1dc36936b5973fc143ca222c1a33ab27b87071a3bbc44d34ca9a",
  },
  FontFile {
    name: "LeelUIsl.ttf",
    size: 386264,
    sha256: "2e2fabd71aa30f0d7039b879ee58441a791e095a6656064eb9d4a84a071e5e8f",
  },
  FontFile {
    name: "LeelaUIb.ttf",
    size: 327420,
    sha256: "16d0c83e99ca3cd56a85966c4ff2c5f0c0f8a5a7140a7594b2ec3e404c8e96a0",
  },
  FontFile {
    name: "LeelawUI.ttf",
    size: 393764,
    sha256: "796dc7e97b357f9c2ad10044d01a86d3fb49cf80e3693011eb055c466c995de0",
  },
  FontFile {
    name: "Nirmala.ttf",
    size: 1535692,
    sha256: "70f53470e5467d6b0dc2af2f58cdf2622629be2a131652615d9ef8c00a410fec",
  },
  FontFile {
    name: "NirmalaB.ttf",
    size: 1490236,
    sha256: "e0b00c65dbf786eadb4345f62e456d7462b960a5b15c9e07fa5c8809481b9a5f",
  },
  FontFile {
    name: "NirmalaS.ttf",
    size: 1568068,
    sha256: "78709a285aef168b5c2d8073dce837359997a14244d6ed904883717d60eed3b9",
  },
  FontFile {
    name: "SansSerifCollection.ttf",
    size: 1336432,
    sha256: "4e6ebd5a1bab4330be223b7d21624bc55a46a38a3a760e1af87d0388f49176f4",
  },
  FontFile {
    name: "SegUIVar.ttf",
    size: 1851000,
    sha256: "27ca1ab4bd5ad3b0404ee6a9a03b143408dbd7e5a4c9e4b2054639c03fc0f682",
  },
  FontFile {
    name: "SegoeIcons.ttf",
    size: 452344,
    sha256: "45cddf62e8993f4f712fdebfba634a772ff3dc862d2a61d59cb39ec32643c705",
  },
  FontFile {
    name: "SimsunExtG.ttf",
    size: 3558996,
    sha256: "6b4db7bc243aace3a872983f5cce0376eb191ba2245a736b871970e6c7fbaf32",
  },
  FontFile {
    name: "SitkaVF-Italic.ttf",
    size: 558964,
    sha256: "d8d27dc02f26f9f4c30a2b86d93e99492eef702a107af42f4df8a4e049c4e1dc",
  },
  FontFile {
    name: "SitkaVF.ttf",
    size: 517100,
    sha256: "be1f89db95982d7ee568de72219457f7330260c4b742707ef3c5b5438a453840",
  },
  FontFile {
    name: "YuGothB.ttc",
    size: 14743864,
    sha256: "9d0437d151ddd907d72ba9ba348d670575a86e4f5cbf018de18645f2353140e5",
  },
  FontFile {
    name: "YuGothL.ttc",
    size: 13984748,
    sha256: "95392428a804b937919526fec91bab64200664f7f22f777211da6bb244988f56",
  },
  FontFile {
    name: "YuGothM.ttc",
    size: 13923436,
    sha256: "14afc5b98fc53165a2f760fcff11f6ed55ab80f20ae46a2a4a08b7af2b9044d5",
  },
  FontFile {
    name: "YuGothR.ttc",
    size: 13851052,
    sha256: "27c9b8e4cc7c5cbd846d1fa978b7713938e8ae766875962c70c3986b004bbc85",
  },
  FontFile {
    name: "arial.ttf",
    size: 1045960,
    sha256: "baa251526d6862712a58e613ef451d8a2b60482142ec6aab1d47fb8e23e21a7c",
  },
  FontFile {
    name: "arialbd.ttf",
    size: 990208,
    sha256: "8df7a2c69fc4044835814899534e5fee6e72f78285b5a6dcb19531142b51d742",
  },
  FontFile {
    name: "arialbi.ttf",
    size: 730804,
    sha256: "94d0872622e6d592f01440b58dac8f5d7e010509bd76bb71cbed71fc5f4dc173",
  },
  FontFile {
    name: "ariali.ttf",
    size: 727152,
    sha256: "090b89742910172c69e1fd3b1814ad4e482a1c712b87d24e96b377beaac3a6d1",
  },
  FontFile {
    name: "ariblk.ttf",
    size: 167592,
    sha256: "10df702864b1f89cb29ba0d6b97c04228338d16807e13e8d8c74b91aba5e5f23",
  },
  FontFile {
    name: "bahnschrift.ttf",
    size: 338468,
    sha256: "6147a218696424127035e25c4be335c73a7d69b052b714d05806b3c17898b49f",
  },
  FontFile {
    name: "calibri.ttf",
    size: 1681220,
    sha256: "df2c69a18a462e5cbc97d04a033f3bd7cd0abfe818381641f8c2dee7b7c43dbd",
  },
  FontFile {
    name: "calibrib.ttf",
    size: 1647428,
    sha256: "5e8b2eb430f543eb304f08f44f5eb02ccbc39ffdc95400aae96f5dcd2aea73d8",
  },
  FontFile {
    name: "calibrii.ttf",
    size: 1228564,
    sha256: "21390a7e84b61224916e810fd00344ad9ad4f1d9115151a551845a4387d20b10",
  },
  FontFile {
    name: "calibril.ttf",
    size: 1471248,
    sha256: "744d3ec1b277e3652ba8f8d45c89f83c99b7ecf3eb2dfa27306d7be56d3ee030",
  },
  FontFile {
    name: "calibrili.ttf",
    size: 1092612,
    sha256: "67ac784ac390f6c48119e76f9e34589f9a86b8974f561a6159545b858ab4cd41",
  },
  FontFile {
    name: "calibriz.ttf",
    size: 1190004,
    sha256: "b699a59faad65272ee48a4d118d6bc743d4cc4ba338cb1c6347deec58aa1e1a0",
  },
  FontFile {
    name: "cambria.ttc",
    size: 1792664,
    sha256: "84e70ccc1664482f4a960442c7a166c91a1b2cf98ff88c33cb73f79403f66d7b",
  },
  FontFile {
    name: "cambriab.ttf",
    size: 841440,
    sha256: "6685c657a7e292083e28f19bbf1a1e90509ce3e1130dbc4ed1c62ba3f9140543",
  },
  FontFile {
    name: "cambriai.ttf",
    size: 892620,
    sha256: "6905f0ac846d1ec4326cd14a46cb7379204940aa293b098ee4c1a69723bbd165",
  },
  FontFile {
    name: "cambriaz.ttf",
    size: 856672,
    sha256: "fa90f376ad37f308a819b341c8185f788d30a1012829278592eab45a41818d1a",
  },
  FontFile {
    name: "comic.ttf",
    size: 245600,
    sha256: "a7da3ef195158acd8432226a21f2940f1806a9c8d079f18d0a9ab475f50f9659",
  },
  FontFile {
    name: "comicbd.ttf",
    size: 229408,
    sha256: "56567bc71e89fd173674c78f02b474123a02561b246526a22600c3a29b2fdc65",
  },
  FontFile {
    name: "comici.ttf",
    size: 226708,
    sha256: "65aece2928246837317e093a11c6df5cbfc67035f1e045c9771f7c408be1faea",
  },
  FontFile {
    name: "comicz.ttf",
    size: 224252,
    sha256: "c691699eaf218e702b2d77c1f62d2c080be6248f1c11018b0b2556834a9cecb1",
  },
  FontFile {
    name: "consola.ttf",
    size: 459180,
    sha256: "c6e6ce8119fdd47ec6a5449a08e2d2ad7f41ea03143aae193068ed9fa58eaebc",
  },
  FontFile {
    name: "consolab.ttf",
    size: 397896,
    sha256: "8fa803e5abc7fff0c78c62cd22d07c705f3273c53312380c36ff484246ed3c1b",
  },
  FontFile {
    name: "consolai.ttf",
    size: 467584,
    sha256: "81e2a95d3c49468029c07e58f71a5f0ca4cfcb617eaf6bd77576fb03aa430bb2",
  },
  FontFile {
    name: "consolaz.ttf",
    size: 407956,
    sha256: "1df27cbcc8a9fb7f843bc24e257b78e8ff93de70e8c1ad2caab8379e7d6a1eab",
  },
  FontFile {
    name: "constan.ttf",
    size: 448376,
    sha256: "495577bf8ef74deab6ecc2e7acf63cd966184148f9bc2044a1068cd58501577e",
  },
  FontFile {
    name: "constanb.ttf",
    size: 451824,
    sha256: "b6c66a1858370fe575629e19d53c05350738bf5105e18ceac07fe9efc9cd008d",
  },
  FontFile {
    name: "constani.ttf",
    size: 448748,
    sha256: "2314a03c53506fa79525bc479ea4be9c036d1ebd86a1ffb5837ca5b43ff51f07",
  },
  FontFile {
    name: "constanz.ttf",
    size: 455564,
    sha256: "9311aadcbd919746f50d222fa0215fb44266d6993221c14a196f0725fdae464f",
  },
  FontFile {
    name: "corbel.ttf",
    size: 277168,
    sha256: "a183b77b655df66793639af7874828fb50f748dc4af57ae40f758156f7f34634",
  },
  FontFile {
    name: "corbelb.ttf",
    size: 290908,
    sha256: "fa008372f8a94945d0daef9daf0bfb9e3fba3a25fd17f2607d47ac6861bd0303",
  },
  FontFile {
    name: "corbeli.ttf",
    size: 284690,
    sha256: "9be778c9ed418770f6b5a02064d9c0b47a8fdea1137618f0bec9ac4072430a45",
  },
  FontFile {
    name: "corbell.ttf",
    size: 172256,
    sha256: "0e36ddf90102e476c6739ca4c8cc18ba8309d880c12558981d2e4d42de78fc00",
  },
  FontFile {
    name: "corbelli.ttf",
    size: 169228,
    sha256: "a031500bccd32b5d6e819699b774fc679c696ed4965b643d51a94eb398475550",
  },
  FontFile {
    name: "corbelz.ttf",
    size: 294692,
    sha256: "588f090ae3a6d31370457312683d14ee3a5bfbdaaa0a070b7c80225b55b23e78",
  },
  FontFile {
    name: "cour.ttf",
    size: 806504,
    sha256: "93c3ff526c19d03496c01c6a1bfd1031f97af021a13e859a89dfc717a0b6b6ec",
  },
  FontFile {
    name: "courbd.ttf",
    size: 805244,
    sha256: "716c08d704f154870bd9fb78b185d0b644912301dfa9c9e6daf45bdb4832a299",
  },
  FontFile {
    name: "courbi.ttf",
    size: 577488,
    sha256: "8fdb188252a2db0f020c89831bddb61cadfe290c3616fe534479b4d3b10d09c1",
  },
  FontFile {
    name: "couri.ttf",
    size: 667440,
    sha256: "5861588608d3a3775d921a7e4acb8362b21630c6b63718bc68adb7137ce53beb",
  },
  FontFile {
    name: "ebrima.ttf",
    size: 921480,
    sha256: "6406c765cfb81bf302d767cb2383a5a51e26e50d7239ed22233dbae689ae1f05",
  },
  FontFile {
    name: "ebrimabd.ttf",
    size: 929968,
    sha256: "9090931d496a272ab9f1382480332e6029e636a9191fe21d158adbf148acf0df",
  },
  FontFile {
    name: "framd.ttf",
    size: 133632,
    sha256: "bac844bd41f847cf138737026d41c983387a1065535c32e78aa22c48708b52ad",
  },
  FontFile {
    name: "framdit.ttf",
    size: 148728,
    sha256: "df59f9ea915957b22a389d68580bf40dc5bd05822cc65b5fdcc188c706a694e7",
  },
  FontFile {
    name: "gadugi.ttf",
    size: 242684,
    sha256: "0ec2bdbfc2ba99d9fb05b6f7c511e7ab8b7c055e2d644570ad777455d6e9796d",
  },
  FontFile {
    name: "gadugib.ttf",
    size: 249236,
    sha256: "c96226cba7f50e94b534a5f8ade6a54f48a03be7c73360524eda5cbce4de34b6",
  },
  FontFile {
    name: "georgia.ttf",
    size: 219712,
    sha256: "38dffcaf1fac89492f7c56c600a14daf36ae42f7c21985cd4134ae113124a58f",
  },
  FontFile {
    name: "georgiab.ttf",
    size: 207476,
    sha256: "72a6cd94fab6c179392075d3fb361e269cdddfad41bb7ab385fa22a37e49a900",
  },
  FontFile {
    name: "georgiai.ttf",
    size: 208444,
    sha256: "d3e4bad27651d120ac8f2cea42cf4b4f077264fb243f66b3caffcfd35465f49b",
  },
  FontFile {
    name: "georgiaz.ttf",
    size: 211780,
    sha256: "3f9e512f82eaf6f1b7869b82013fb79f6eaf1a41e75a0e7b96f31f35b1022ae4",
  },
  FontFile {
    name: "himalaya.ttf",
    size: 572776,
    sha256: "fdec3a7efacd57913d3c14111837908ccbabea5b0b44ec62527eb73acf743afb",
  },
  FontFile {
    name: "impact.ttf",
    size: 206784,
    sha256: "d8bbdb72e3f930157824b2e59677d3a912d4512c14a948e6656a2e3fb0c935ad",
  },
  FontFile {
    name: "javatext.ttf",
    size: 305448,
    sha256: "df8d94382f73e6d2197b60653c3d55c28ab5cf127a82e65b8af4ed1b52fdc412",
  },
  FontFile {
    name: "l_10646.ttf",
    size: 309516,
    sha256: "97226e81f19eff8c8fb191745748bab920472c005d3ec4e23d9a50a12c471d92",
  },
  FontFile {
    name: "lucon.ttf",
    size: 111108,
    sha256: "2ca2cfde263fa420e8d39420f075ee9ac80bc0794272c53ac4571a88f8a075a6",
  },
  FontFile {
    name: "malgun.ttf",
    size: 13457164,
    sha256: "0086c19e81d293a542e7d75564c645fb58070cc850aefebf8fa1c397858e510c",
  },
  FontFile {
    name: "malgunbd.ttf",
    size: 12598360,
    sha256: "a541834fb9cdde9bd6d2c3ff1565cb0bb5ccf7a15e4e9daf2cb58d9b1cf282aa",
  },
  FontFile {
    name: "malgunsl.ttf",
    size: 5083156,
    sha256: "a7217a2e590c86141bddadc74a2bcdff2efe210db48469a77a749e0446d91ea9",
  },
  FontFile {
    name: "micross.ttf",
    size: 888236,
    sha256: "b41c5227ddfca092a57603067ae59933473b1a63d54c9cc90b174e5741088853",
  },
  FontFile {
    name: "mingliub.ttc",
    size: 36786404,
    sha256: "814fc6cc5407d2137022a1ff8587cfa43d5a2736b2ab5378ea49ceb5c072de37",
  },
  FontFile {
    name: "mmrtext.ttf",
    size: 354468,
    sha256: "37598a7b023c80f3e56c44ed91e01936a96b3e81c888bee82f85113da39f9468",
  },
  FontFile {
    name: "mmrtextb.ttf",
    size: 336328,
    sha256: "b4329b395966137485b324d0c15817e99f0167d559d81a01e90d8761d52261b9",
  },
  FontFile {
    name: "monbaiti.ttf",
    size: 289052,
    sha256: "707585b9acfc1507d9bd943bb5435f75768274b1a12b1f71de76ad78be4d7e7b",
  },
  FontFile {
    name: "msgothic.ttc",
    size: 8990160,
    sha256: "4bde3e6392b96910fb59094c6c1a4dbfae18fee78d0bf13dc30616837c4f95db",
  },
  FontFile {
    name: "msjh.ttc",
    size: 21402152,
    sha256: "0b50839f0e13f9e1607b46568aa953fc1491eb3f867f2737e6a4ff9eb3696aa9",
  },
  FontFile {
    name: "msjhbd.ttc",
    size: 14441192,
    sha256: "73293967f4bb2ceb68a40d642d7dbe410ea55abb1410ba94c02d9d88e4d055bf",
  },
  FontFile {
    name: "msjhl.ttc",
    size: 12875740,
    sha256: "d3b01af9bfc265ee4057b76c897243ed42f14d62a78b7306084e54dba37a548e",
  },
  FontFile {
    name: "msyh.ttc",
    size: 19701556,
    sha256: "3084f1f88369af6bf9989c909024164d953d1e38d08734f05f28ef24b2f9d577",
  },
  FontFile {
    name: "msyhbd.ttc",
    size: 16880832,
    sha256: "519309b7ab0479c4dc3ace5e291de5a8702175be5586e165bc810267bd4619a5",
  },
  FontFile {
    name: "msyhl.ttc",
    size: 12183612,
    sha256: "f25f39a6d56ec6d1b356254dd776219d71b7e6f3584b3ec1b83aab0ceb08881a",
  },
  FontFile {
    name: "msyi.ttf",
    size: 301180,
    sha256: "80aef1c25135b945e9de7d09e6445c386405eb2e0ea7032d4cd00a2fe3eef0ae",
  },
  FontFile {
    name: "mvboli.ttf",
    size: 79320,
    sha256: "5dfd7a4c708caeefae42cdb9540946de042beeb8c069dfde0dbc14c2fa0705ae",
  },
  FontFile {
    name: "ntailu.ttf",
    size: 74464,
    sha256: "de0b632015447f28c2db41c8e07c745cedb05ead5dbc2e73f19da14d2e5f2151",
  },
  FontFile {
    name: "ntailub.ttf",
    size: 67168,
    sha256: "cfc2acbf9ae0205a0a258e564bdbf5466a4e49a95281a1d2e72690f149ec837b",
  },
  FontFile {
    name: "pala.ttf",
    size: 472284,
    sha256: "e47d594e2e2bc31b28f87b6f66483d32f925843ceb002f919c7a403aca48813d",
  },
  FontFile {
    name: "palab.ttf",
    size: 419932,
    sha256: "8e14668bbae03f78ed668dbe3261218881960dcd9fb80ad6c1d1dfb4226a57c3",
  },
  FontFile {
    name: "palabi.ttf",
    size: 335292,
    sha256: "d0e722b91ffd5391cc2e7d6e976f274d5aee9fe0b8d0fedfffe093763e695cde",
  },
  FontFile {
    name: "palai.ttf",
    size: 413708,
    sha256: "99f0af2d1941707fecda23da38bafbed5144a73a3720d773c05b9d4c445920ed",
  },
  FontFile {
    name: "phagspa.ttf",
    size: 93720,
    sha256: "538949482d964e9f353a3a8d860bb3eda93b46b830f3b677e4f53eb2ff3a6e7d",
  },
  FontFile {
    name: "phagspab.ttf",
    size: 97476,
    sha256: "03e4e45bb8ebb6d055a5584166cc26712bf7e746ea29cb6e548341361d31dc85",
  },
  FontFile {
    name: "segmdl2.ttf",
    size: 283820,
    sha256: "a2efc8b8e51fbd031a7142a1ade8866fa904c0c349511abc579ee53f8a926f31",
  },
  FontFile {
    name: "segoepr.ttf",
    size: 168404,
    sha256: "91046862e08fbade0bdd648e81c4efbe6a963947f8b6e32b64a28fe39073c468",
  },
  FontFile {
    name: "segoeprb.ttf",
    size: 167800,
    sha256: "3cced0162ddb9e5f1e63ee1b36fd6e581c090318dce1a0af01b77f5ea9952c80",
  },
  FontFile {
    name: "segoesc.ttf",
    size: 597476,
    sha256: "b24aa49a4bcb99a0ffc60cad77f00941e4b121153d8b0a519c03719d9be6004e",
  },
  FontFile {
    name: "segoescb.ttf",
    size: 593028,
    sha256: "6cd482205e1390b32a2b6327e1e5039424e346172c187074eac12006bc51935e",
  },
  FontFile {
    name: "segoeui.ttf",
    size: 975088,
    sha256: "74f2b3d0c20cf7380eb121a09fd7cdfdc1ccdd12a00db83caec0feb48b4db9f7",
  },
  FontFile {
    name: "segoeuib.ttf",
    size: 964768,
    sha256: "d105038ae445a7ab3e7c037eae9c6a436f71f603136f353e8338cfca40e6ca18",
  },
  FontFile {
    name: "segoeuii.ttf",
    size: 532220,
    sha256: "821ee79ea06ea1ce24964baf760d740086e8e1e6b88528978307cf1729cfaf1d",
  },
  FontFile {
    name: "segoeuil.ttf",
    size: 925552,
    sha256: "fe28fdf790453728a07233877c0a708477319fa798363ae4a1657d04fd173416",
  },
  FontFile {
    name: "segoeuisl.ttf",
    size: 863836,
    sha256: "483bb4800d30031ae1970dbd44a64f2f476a9fcba326bd0a216dac6e0b92e333",
  },
  FontFile {
    name: "segoeuiz.ttf",
    size: 544004,
    sha256: "79872dff519fde7a0c7cc8ed4f454a5fd7fd7628c527dc74473056a32a9eeb38",
  },
  FontFile {
    name: "seguibl.ttf",
    size: 324260,
    sha256: "e17738f092c8b02f4443867a7dfcdde66fb4cd6f6b10de8e40b2f3192f8a5835",
  },
  FontFile {
    name: "seguibli.ttf",
    size: 356008,
    sha256: "33212faa85fad61785cd6917172378b798bc9f5d4c47c121437354f47980aa6f",
  },
  FontFile {
    name: "seguiemj.ttf",
    size: 12417712,
    sha256: "12c5253251f45c57fa57e2a1c748f821d3ca030a3e757e049a5da6316f213bcb",
  },
  FontFile {
    name: "seguihis.ttf",
    size: 1606568,
    sha256: "fec0897934d26625b17bb3e4ffb3a7b0b6a138f6e61d5eff7535417cfbbc7884",
  },
  FontFile {
    name: "seguili.ttf",
    size: 461608,
    sha256: "d3366c28fc5d73644465fe08bc2d8571710955238e525dec4c9d6b5d22fb92fa",
  },
  FontFile {
    name: "seguisb.ttf",
    size: 991152,
    sha256: "b381730d47408ced8f104b62c9042a6abbbd08501a37f14b76858760a6cf176c",
  },
  FontFile {
    name: "seguisbi.ttf",
    size: 458708,
    sha256: "60cd474595d07bbdfa9d87de1a1e24c876e218d117c4a3098e0ac937157a1b9d",
  },
  FontFile {
    name: "seguisli.ttf",
    size: 438896,
    sha256: "254ad309e8f6b49cf0379a565adaf85a78994b4420b98a94119f45449d7b70ab",
  },
  FontFile {
    name: "seguisym.ttf",
    size: 2454728,
    sha256: "d2f8326a354456d93e78b0537c58793e7072c3617af7d9ee187f10d6d595f510",
  },
  FontFile {
    name: "simsun.ttc",
    size: 18008680,
    sha256: "05068fa1acae212120dcc7e98c884b174a6eca252a636b4b64030235473ec27e",
  },
  FontFile {
    name: "simsunb.ttf",
    size: 21877944,
    sha256: "d75bef7991a4acc6aec7be5cea3698d97d7a7fd791050a64a8cb926731dfcfa2",
  },
  FontFile {
    name: "sylfaen.ttf",
    size: 259980,
    sha256: "55be3490211845a51960c129be1d793782765383b2dfcc969228d4ab5ae8fe64",
  },
  FontFile {
    name: "symbol.ttf",
    size: 69036,
    sha256: "bbf1192965e5266fbf23370416337d2861fc1e8bd349def93f2994ba67382fc3",
  },
  FontFile {
    name: "tahoma.ttf",
    size: 951868,
    sha256: "f936530a7ede296580f897c47e7a3fa48a9483166080ce05105673a1339cbf0c",
  },
  FontFile {
    name: "tahomabd.ttf",
    size: 880644,
    sha256: "a05f9206c44ba5795690f03fc767f5e5209bf913e23cf61f45d2ed84c3a424e5",
  },
  FontFile {
    name: "taile.ttf",
    size: 67568,
    sha256: "f5151d081f6afea4c26464e0cd5e695f57a99209cfeea2f483c3108684660eb6",
  },
  FontFile {
    name: "taileb.ttf",
    size: 58532,
    sha256: "1878842d8dbdf8489794af0bace95a956db270dfa506ddcaabf94fa827b769ac",
  },
  FontFile {
    name: "times.ttf",
    size: 1206884,
    sha256: "43a890795ad133d73568d169a35924f40416be8a6950a1a03263052ec8e95b8a",
  },
  FontFile {
    name: "timesbd.ttf",
    size: 1185876,
    sha256: "56735cfcb1d3feac66d5d9bb446c8598e522dc03c54923943899217c227bdfab",
  },
  FontFile {
    name: "timesbi.ttf",
    size: 918940,
    sha256: "914ae3beb26a5948dbf67d87a7f86f10abe9407d420fa6a448f974e41d0f3727",
  },
  FontFile {
    name: "timesi.ttf",
    size: 977892,
    sha256: "f3a1bbc53e7c3c21262f033c4d73a55a990952cbd4644ea42cb9f0c4c4640e40",
  },
  FontFile {
    name: "trebuc.ttf",
    size: 254396,
    sha256: "fe3e8bdd64c1c9f3b9cf5189d06510c80d72cec28e03d04bffc560f77eec58d1",
  },
  FontFile {
    name: "trebucbd.ttf",
    size: 244124,
    sha256: "dfcc306777a594d2815368547dd897e2ac82aff8073e55d9e682569939e49b81",
  },
  FontFile {
    name: "trebucbi.ttf",
    size: 225868,
    sha256: "87942234ab0cc4400088648bef8d1f7afc4c44e676c7ed6b32ca31462c18307d",
  },
  FontFile {
    name: "trebucit.ttf",
    size: 252224,
    sha256: "9c9dc0cd0fd6b1a06e8f406d04bb15a88aaf20328d3e166c12d6bc7994b781d2",
  },
  FontFile {
    name: "verdana.ttf",
    size: 243304,
    sha256: "6a8481fe107ee547893c018b13dba291c2020bec3de5da6525d9ac09f6bc2105",
  },
  FontFile {
    name: "verdanab.ttf",
    size: 211260,
    sha256: "f3245f5f38f61bd1ceefb0f1338a5b88a21e6220832c2f43a38bbc7e1547c36f",
  },
  FontFile {
    name: "verdanai.ttf",
    size: 223268,
    sha256: "14f2ebcdaa1b77e5993a8533662e4bd3ebfe2c5771b6daafd8ccb39b2ad588b9",
  },
  FontFile {
    name: "verdanaz.ttf",
    size: 229800,
    sha256: "d71f81c83ec63eaa32d36d5df7be1d9e71d3ea9150f47cebda2924923cbbf18a",
  },
  FontFile {
    name: "webdings.ttf",
    size: 120560,
    sha256: "67461fabd8329d0d8ad62689cac642b34fac81c754ff9077ad211160ec62282d",
  },
  FontFile {
    name: "wingding.ttf",
    size: 82180,
    sha256: "9bcd6064e3b52fa30a347edd6e82506e2ec8b1602930b423c629a82c10468a5b",
  },
];

fn expected() -> impl Iterator<Item = &'static FontFile> {
  WINDOWS_FONTS.iter().chain(std::iter::once(&MARLETT))
}

/// Where a persona of `platform` takes its fonts from on this machine, or
/// `None` when it needs none (the machine's own OS) or the OS has no per-user
/// font directory Donut can fill.
pub fn fonts_dir(platform: &str) -> Option<PathBuf> {
  if platform == crate::profile::types::get_host_os() {
    return None;
  }
  let name = match platform {
    "windows" => "apostate-windows",
    "macos" => "apostate-macos",
    _ => return None,
  };
  if cfg!(target_os = "macos") {
    dirs::home_dir().map(|home| home.join("Library/Fonts").join(name))
  } else if cfg!(target_os = "linux") {
    dirs::home_dir().map(|home| home.join(".local/share/fonts").join(name))
  } else {
    None
  }
}

fn missing(dir: &Path) -> Vec<&'static FontFile> {
  expected()
    .filter(|font| {
      std::fs::metadata(dir.join(font.name)).map_or(true, |meta| meta.len() != font.size)
    })
    .collect()
}

/// Whether the persona's fonts are installed on this machine. A persona without
/// them still launches, but a page that measures text sees the host's fonts.
pub fn present(platform: &str) -> bool {
  match fonts_dir(platform) {
    Some(dir) if platform == "windows" => missing(&dir).is_empty(),
    Some(dir) => std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some()),
    None => true,
  }
}

/// No download in unit tests or an end-to-end session: neither may write to
/// the machine's font directory or fetch 350 MB.
fn disabled() -> bool {
  cfg!(test) || crate::e2e_automation_enabled()
}

/// What the `persona-fonts-progress` event carries while the fonts install.
#[derive(Debug, Clone, Serialize)]
struct Progress {
  /// "downloading", "completed" or "error".
  stage: &'static str,
  percentage: f64,
  speed_bytes_per_sec: f64,
  eta_seconds: Option<f64>,
}

fn emit(progress: Progress) {
  let _ = crate::events::emit("persona-fonts-progress", &progress);
}

fn finished(stage: &'static str) -> Progress {
  Progress {
    stage,
    percentage: 100.0,
    speed_bytes_per_sec: 0.0,
    eta_seconds: None,
  }
}

static INSTALL: LazyLock<AsyncMutex<()>> = LazyLock::new(|| AsyncMutex::new(()));
static INSTALL_STREAK: Streak = Streak::new(module_path!(), "Persona fonts install");

/// The fonts a persona of `platform` needs, downloaded first when they are not
/// all on disk. One install at a time: a launch and the background install
/// share it. Only the Windows fonts can be downloaded; for any other persona
/// this does nothing.
pub async fn ensure_installed(platform: &str) -> Result<(), String> {
  if platform != "windows" || disabled() {
    return Ok(());
  }
  let Some(dir) = fonts_dir(platform) else {
    return Ok(());
  };
  if missing(&dir).is_empty() {
    return Ok(());
  }
  let _install = INSTALL.lock().await;
  let wanted = missing(&dir);
  if wanted.is_empty() {
    return Ok(());
  }
  let started = std::time::Instant::now();
  let bytes: u64 = wanted.iter().map(|font| font.size).sum();
  match install(&dir, &wanted).await {
    Ok(()) => {
      emit(finished("completed"));
      INSTALL_STREAK.succeeded();
      log::info!(
        "Persona fonts installed platform=windows files={} bytes={bytes} elapsed_ms={}",
        wanted.len(),
        started.elapsed().as_millis()
      );
      Ok(())
    }
    Err(error) => {
      emit(finished("error"));
      INSTALL_STREAK.failed(format!(
        "platform=windows files={} err=\"{}\"",
        wanted.len(),
        crate::log_redaction::text(&error)
      ));
      Err(error)
    }
  }
}

/// Installs the Windows fonts until they are on disk, with the engine's
/// growing wait between failed attempts. Called after the engine's own
/// background install, so the engine downloads first.
pub async fn keep_installing() {
  if disabled() || fonts_dir("windows").is_none() {
    return;
  }
  let mut failures = 0u32;
  while ensure_installed("windows").await.is_err() {
    tokio::time::sleep(crate::apostate::retry_delay(failures)).await;
    failures += 1;
  }
}

struct Meter {
  total: u64,
  done: AtomicU64,
  began: std::time::Instant,
  last_emit: std::sync::Mutex<std::time::Instant>,
}

impl Meter {
  fn add(&self, bytes: u64) {
    let done = self.done.fetch_add(bytes, Ordering::Relaxed) + bytes;
    let Ok(mut last) = self.last_emit.try_lock() else {
      return;
    };
    if last.elapsed() < std::time::Duration::from_millis(250) {
      return;
    }
    *last = std::time::Instant::now();
    let speed = done as f64 / self.began.elapsed().as_secs_f64().max(0.001);
    emit(Progress {
      stage: "downloading",
      percentage: (done as f64 / self.total.max(1) as f64 * 100.0).min(100.0),
      speed_bytes_per_sec: speed,
      eta_seconds: (speed > 0.0).then(|| self.total.saturating_sub(done) as f64 / speed),
    });
  }
}

fn client() -> Result<reqwest::Client, String> {
  crate::app_proxy::client_builder()
    .connect_timeout(std::time::Duration::from_secs(30))
    .read_timeout(std::time::Duration::from_secs(60))
    .build()
    .map_err(|e| e.to_string())
}

async fn install(dir: &Path, wanted: &[&'static FontFile]) -> Result<(), String> {
  let staging = crate::app_dirs::binaries_dir().join(".persona-fonts-staging");
  let _ = tokio::fs::remove_dir_all(&staging).await;
  tokio::fs::create_dir_all(&staging)
    .await
    .map_err(|e| e.to_string())?;
  tokio::fs::create_dir_all(dir)
    .await
    .map_err(|e| e.to_string())?;
  let client = client()?;
  let meter = Arc::new(Meter {
    total: wanted.iter().map(|font| font.size).sum(),
    done: AtomicU64::new(0),
    began: std::time::Instant::now(),
    last_emit: std::sync::Mutex::new(std::time::Instant::now()),
  });
  let mut queue = wanted.iter().copied();
  let mut running = tokio::task::JoinSet::new();
  let mut result = Ok(());
  loop {
    while result.is_ok() && running.len() < PARALLEL_DOWNLOADS {
      let Some(font) = queue.next() else { break };
      running.spawn(download(
        client.clone(),
        font,
        meter.clone(),
        staging.clone(),
        dir.to_path_buf(),
      ));
    }
    let Some(done) = running.join_next().await else {
      break;
    };
    if let (Ok(()), Err(error)) = (&result, done.map_err(|e| e.to_string()).and_then(|r| r)) {
      result = Err(error);
      running.abort_all();
    }
  }
  let _ = tokio::fs::remove_dir_all(&staging).await;
  result?;
  if cfg!(target_os = "linux") {
    refresh_font_cache(dir).await;
  }
  Ok(())
}

async fn download(
  client: reqwest::Client,
  font: &'static FontFile,
  meter: Arc<Meter>,
  staging: PathBuf,
  dir: PathBuf,
) -> Result<(), String> {
  let data = if font.name == MARLETT.name {
    let data = fetch_marlett(&client).await?;
    meter.add(data.len() as u64);
    data
  } else {
    fetch_file(&client, font, &meter).await?
  };
  verify(font, &data)?;
  place(&staging, &dir, font.name, &data).await
}

async fn fetch_file(
  client: &reqwest::Client,
  font: &FontFile,
  meter: &Meter,
) -> Result<Vec<u8>, String> {
  use futures_util::StreamExt;
  let response = client
    .get(format!("{FONTS_BASE}/{}", font.name))
    .send()
    .await
    .and_then(|response| response.error_for_status())
    .map_err(|e| format!("{}: {e}", font.name))?;
  let mut data = Vec::with_capacity(font.size as usize);
  let mut stream = response.bytes_stream();
  while let Some(chunk) = stream.next().await {
    let chunk = chunk.map_err(|e| format!("{}: {e}", font.name))?;
    if data.len() + chunk.len() > font.size as usize {
      return Err(format!("{}: larger than {} bytes", font.name, font.size));
    }
    data.extend_from_slice(&chunk);
    meter.add(chunk.len() as u64);
  }
  Ok(data)
}

fn sha256_hex(data: &[u8]) -> String {
  use sha2::{Digest, Sha256};
  use std::fmt::Write;
  let mut hex = String::with_capacity(64);
  for byte in Sha256::digest(data) {
    let _ = write!(hex, "{byte:02x}");
  }
  hex
}

fn verify(font: &FontFile, data: &[u8]) -> Result<(), String> {
  if data.len() as u64 != font.size || sha256_hex(data) != font.sha256 {
    return Err(format!("{}: checksum mismatch", font.name));
  }
  Ok(())
}

/// Writes a verified file beside the fonts first, so the font directory never
/// holds a partial file the OS could try to load.
async fn place(staging: &Path, dir: &Path, name: &str, data: &[u8]) -> Result<(), String> {
  let staged = staging.join(name);
  tokio::fs::write(&staged, data)
    .await
    .map_err(|e| e.to_string())?;
  let destination = dir.join(name);
  if tokio::fs::rename(&staged, &destination).await.is_err() {
    let partial = dir.join(format!(".{name}.part"));
    tokio::fs::copy(&staged, &partial)
      .await
      .map_err(|e| e.to_string())?;
    tokio::fs::rename(&partial, &destination)
      .await
      .map_err(|e| e.to_string())?;
  }
  Ok(())
}

async fn refresh_font_cache(dir: &Path) {
  match tokio::process::Command::new("fc-cache")
    .arg("-f")
    .arg(dir)
    .output()
    .await
  {
    Ok(output) if output.status.success() => {}
    Ok(output) => log::warn!("fc-cache failed status={}", output.status),
    Err(e) => log::warn!("fc-cache unavailable err=\"{e}\""),
  }
}

/// `length` bytes of `url` from `offset`, and the size of the whole file.
async fn read_range(
  client: &reqwest::Client,
  url: &str,
  offset: u64,
  length: u64,
) -> Result<(Vec<u8>, u64), String> {
  let response = client
    .get(url)
    .header(
      reqwest::header::RANGE,
      format!("bytes={offset}-{}", offset + length - 1),
    )
    .send()
    .await
    .map_err(|e| e.to_string())?;
  if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
    return Err(format!("byte range refused status={}", response.status()));
  }
  let size = response
    .headers()
    .get(reqwest::header::CONTENT_RANGE)
    .and_then(|value| value.to_str().ok())
    .and_then(|value| value.rsplit('/').next())
    .and_then(|size| size.trim().parse::<u64>().ok())
    .ok_or("byte range without a file size")?;
  let data = response.bytes().await.map_err(|e| e.to_string())?;
  if data.len() as u64 != length {
    return Err("wrong byte range".to_string());
  }
  Ok((data.to_vec(), size))
}

fn u16_at(data: &[u8], at: usize) -> Option<u64> {
  Some(u16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?) as u64)
}

fn u32_at(data: &[u8], at: usize) -> Option<u64> {
  Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?) as u64)
}

/// A zip's central directory from its end record: entry count, size, offset.
fn central_directory(tail: &[u8]) -> Option<(u64, u64, u64)> {
  let end = tail.windows(4).rposition(|w| w == b"PK\x05\x06")?;
  let record = tail.get(end..end + 22)?;
  let (entries, size, offset) = (
    u16_at(record, 10)?,
    u32_at(record, 12)?,
    u32_at(record, 16)?,
  );
  (size != 0xFFFF_FFFF && offset != 0xFFFF_FFFF).then_some((entries, size, offset))
}

#[derive(Debug, PartialEq)]
struct ZipEntry {
  method: u64,
  compressed: u64,
  uncompressed: u64,
  local_header: u64,
}

/// The entry whose file name (any directory) is `name`, case-insensitively.
fn find_entry(central: &[u8], entries: u64, name: &str) -> Option<ZipEntry> {
  let mut at = 0usize;
  for _ in 0..entries {
    if central.get(at..at + 4)? != b"PK\x01\x02" {
      return None;
    }
    let name_length = u16_at(central, at + 28)? as usize;
    let skip =
      name_length + u16_at(central, at + 30)? as usize + u16_at(central, at + 32)? as usize;
    let file = central.get(at + 46..at + 46 + name_length)?;
    let file = String::from_utf8_lossy(file);
    if file.rsplit('/').next()?.eq_ignore_ascii_case(name) {
      return Some(ZipEntry {
        method: u16_at(central, at + 10)?,
        compressed: u32_at(central, at + 20)?,
        uncompressed: u32_at(central, at + 24)?,
        local_header: u32_at(central, at + 42)?,
      });
    }
    at += 46 + skip;
  }
  None
}

fn inflate(method: u64, data: Vec<u8>, size: u64) -> Result<Vec<u8>, String> {
  use std::io::Read;
  match method {
    0 => Ok(data),
    8 => {
      let mut out = Vec::with_capacity(size as usize);
      flate2::read::DeflateDecoder::new(data.as_slice())
        .take(size + 1)
        .read_to_end(&mut out)
        .map_err(|e| e.to_string())?;
      Ok(out)
    }
    other => Err(format!("zip compression method {other} unsupported")),
  }
}

/// Windows 11's marlett.ttf out of `MARLETT_ZIP`: its central directory and
/// the one entry are downloaded, nothing else.
async fn fetch_marlett(client: &reqwest::Client) -> Result<Vec<u8>, String> {
  let fail = |detail: &str| format!("marlett.ttf: {detail}");
  let (_, size) = read_range(client, MARLETT_ZIP, 0, 1).await?;
  let start = size.saturating_sub(ZIP_TAIL);
  let (tail, _) = read_range(client, MARLETT_ZIP, start, size - start).await?;
  let (entries, cd_size, cd_offset) =
    central_directory(&tail).ok_or_else(|| fail("no zip end record"))?;
  let central = if cd_offset >= start {
    tail
      .get((cd_offset - start) as usize..(cd_offset - start + cd_size) as usize)
      .ok_or_else(|| fail("damaged central directory"))?
      .to_vec()
  } else {
    read_range(client, MARLETT_ZIP, cd_offset, cd_size).await?.0
  };
  let entry = find_entry(&central, entries, MARLETT.name).ok_or_else(|| fail("not in the zip"))?;
  if entry.uncompressed != MARLETT.size || entry.compressed > MARLETT.size + 1024 {
    return Err(fail("unexpected size"));
  }
  let (local, _) = read_range(client, MARLETT_ZIP, entry.local_header, 30).await?;
  if local.get(..4) != Some(b"PK\x03\x04".as_slice()) {
    return Err(fail("damaged local header"));
  }
  let skip = u16_at(&local, 26).unwrap_or(0) + u16_at(&local, 28).unwrap_or(0);
  let (data, _) = read_range(
    client,
    MARLETT_ZIP,
    entry.local_header + 30 + skip,
    entry.compressed,
  )
  .await?;
  inflate(entry.method, data, MARLETT.size)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn every_pinned_font_is_a_sha256_and_has_a_plain_name() {
    let mut names = std::collections::HashSet::new();
    for font in expected() {
      assert_eq!(font.sha256.len(), 64, "{}", font.name);
      assert!(font.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
      assert!(font.size > 0);
      assert!(
        crate::app_dirs::is_plain_path_part(font.name),
        "{}",
        font.name
      );
      assert!(
        names.insert(font.name.to_ascii_lowercase()),
        "{}",
        font.name
      );
    }
    for required in ["SegUIVar.ttf", "SitkaVF.ttf", "segoeui.ttf", "arial.ttf"] {
      assert!(names.contains(&required.to_ascii_lowercase()), "{required}");
    }
  }

  #[test]
  fn a_font_set_counts_only_when_every_file_has_its_size() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(missing(dir.path()).len(), expected().count());
    for font in expected() {
      std::fs::write(dir.path().join(font.name), vec![0u8; font.size as usize]).unwrap();
    }
    assert!(missing(dir.path()).is_empty());
    std::fs::write(dir.path().join(MARLETT.name), b"short").unwrap();
    let left: Vec<_> = missing(dir.path()).iter().map(|font| font.name).collect();
    assert_eq!(left, vec![MARLETT.name]);
  }

  #[test]
  fn a_file_is_kept_only_with_its_pinned_digest() {
    let font = FontFile {
      name: "a.ttf",
      size: 3,
      sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
    };
    assert!(verify(&font, b"abc").is_ok());
    assert!(verify(&font, b"abx").is_err());
    assert!(verify(&font, b"abcd").is_err());
  }

  #[test]
  fn the_marlett_entry_is_found_and_inflated_from_the_central_directory() {
    use std::io::Write;
    let body: Vec<u8> = (0..4000u32).flat_map(|n| (n % 7).to_le_bytes()).collect();
    let mut buffer = std::io::Cursor::new(Vec::new());
    {
      let mut zip = zip::ZipWriter::new(&mut buffer);
      let deflated = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
      zip.start_file("Fonts/arial.ttf", deflated).unwrap();
      zip.write_all(b"not this one").unwrap();
      zip.start_file("Fonts/MARLETT.TTF", deflated).unwrap();
      zip.write_all(&body).unwrap();
      zip.finish().unwrap();
    }
    let archive = buffer.into_inner();
    let (entries, size, offset) = central_directory(&archive).unwrap();
    assert_eq!(entries, 2);
    let central = &archive[offset as usize..(offset + size) as usize];
    let entry = find_entry(central, entries, "marlett.ttf").unwrap();
    assert_eq!(entry.uncompressed, body.len() as u64);
    assert_eq!(entry.method, 8);
    let local = &archive[entry.local_header as usize..];
    assert_eq!(&local[..4], b"PK\x03\x04");
    let start = entry.local_header as usize
      + 30
      + (u16_at(local, 26).unwrap() + u16_at(local, 28).unwrap()) as usize;
    let data = archive[start..start + entry.compressed as usize].to_vec();
    assert_eq!(
      inflate(entry.method, data, entry.uncompressed).unwrap(),
      body
    );
    assert!(find_entry(central, entries, "symbol.ttf").is_none());
  }

  #[tokio::test]
  #[ignore = "downloads from GitHub"]
  async fn the_pinned_sources_still_serve_the_pinned_files() {
    let dir = tempfile::tempdir().unwrap();
    let small = WINDOWS_FONTS.iter().min_by_key(|font| font.size).unwrap();
    install(dir.path(), &[small, &MARLETT]).await.unwrap();
    for font in [small, &MARLETT] {
      verify(font, &std::fs::read(dir.path().join(font.name)).unwrap()).unwrap();
    }
  }

  #[test]
  fn only_a_windows_persona_off_windows_downloads_fonts() {
    assert!(fonts_dir(&crate::profile::types::get_host_os()).is_none());
    assert!(fonts_dir("linux").is_none());
    assert!(fonts_dir("android").is_none());
  }
}
