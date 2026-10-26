# kde-desktop-mcp

KDE Plasma (Wayland), Hyprland ve Windows için ekran görüntüsü almadan çalışan bir
masaüstü kontrol MCP sunucusu. Claude Code gibi MCP istemcilerine
uygulamaların içini erişilebilirlik ağacından okuma ve öğelere doğrudan işlem
yapma yeteneği verir. Rust ile yazılmıştır; tek bir çalıştırılabilir dosyadır.

*A screenshot-free desktop control MCP server for KDE Plasma (Wayland), Hyprland
and Windows, built on the platform accessibility APIs, with a per-app allowlist.*

> KDE Plasma gerçek masaüstünde doğrulanmıştır. Hyprland, KDE içinde iç içe
> çalışan bir oturumda doğrulanmıştır. Windows'ta klavye, fare ve ekran
> görüntüsü Wine'da denenmiştir; ağaç okuma (`tree`, `find`, `get_text`,
> `focus`, `click`) henüz gerçek Windows'ta denenmemiştir. Sonuçları
> `tools/mcp_smoke_test.py` ile paylaşın.

## Nasıl çalışır

Claude ekrana bakmak yerine pencerelerdeki öğeleri `[55] button "Kaydet"` gibi
bir liste olarak okur ve öğelere numarasıyla doğrudan işlem yapar.

| Katman | KDE Plasma (Wayland) | Hyprland | Windows |
|---|---|---|---|
| Okuma | AT-SPI (D-Bus) | AT-SPI (D-Bus) | UI Automation önbellek istekleri |
| Öğe işlemleri | AT-SPI eylemleri, EditableText | AT-SPI eylemleri, EditableText | Invoke, Toggle, ExpandCollapse, SelectionItem, Value desenleri |
| Klavye | `/dev/uinput` | `/dev/uinput` | Win32 `SendInput` |
| Fare | Mutlak konumlu uinput | Hyprland `movecursor` + uinput tuşları | `SetCursorPos` + `SendInput` |
| Metin yazma | Qt'nin çok satırlı alanlarına AT-SPI ile doğrudan ekleme (panoya dokunmaz); diğerlerinde süreç içi pano + Ctrl+V | `wtype` (panoya dokunmaz); yoksa `wl-copy` | `KEYEVENTF_UNICODE` (panoya dokunmaz) |
| Pencere | KWin betikleri (D-Bus) | Hyprland soketi: `clients`, `focuswindow` | `SetForegroundWindow` |
| Ekran görüntüsü (yedek) | KWin `ScreenShot2` (D-Bus); yoksa `spectacle` | `grim` | GDI |

Kod `rust/src/` altındadır: `main.rs` MCP protokolünü, izin listesini, öğe
numaralarını ve araçları; `render.rs` ağaç ve arama çıktısını; `backend.rs`
platform arayüzünü tanımlar. Platform kodu `linux/` (KDE ve Hyprland ortak
AT-SPI katmanıyla) ve `windows/` altındadır. Claude her platformda aynı
araçları görür.

Masaüstü otomatik seçilir: Windows'ta Windows, Linux'ta
`HYPRLAND_INSTANCE_SIGNATURE` tanımlıysa Hyprland, değilse KDE. Linux'ta elle
seçmek için `DESKTOP_MCP_BACKEND` ortam değişkenine `kde` ya da `hyprland`
verin.

Hyprland'de etkin pencere, AT-SPI'deki uygulamayla süreç kimliği (pid)
üzerinden eşleştirilir; böylece araç takımının odak bayrağına güvenmek
gerekmez.

`focus` önce erişilebilirlik altyapısından odak ister. Uygulama bunu reddederse
(Qt'de sık görülür) öğenin ortasına gerçek bir tıklama yapar.

Neden hızlı:
- AT-SPI'ye libatspi olmadan doğrudan D-Bus ile konuşur ve istekleri
  beklemeden art arda gönderir (pipelining): bir öğenin bütün öznitelikleri ve
  ağacın her seviyesi aynı anda istenir; süre gidiş-dönüş gecikmesiyle değil
  uygulamanın yanıt hızıyla sınırlanır.
- Windows'ta bir pencerenin bütün alt ağacı, gereken özellikleriyle tek bir
  UI Automation önbellek isteğiyle (`BuildUpdatedCache`) gelir.
- KWin betikleri `qdbus6` süreçleri yerine D-Bus'tan, Hyprland komutları
  `hyprctl` süreçleri yerine doğrudan Hyprland'in soketinden gider.
- Açılış 2 ms; sanal klavye ve fare açılışta, aynı anda hazırlanır.
- KDE'de bekleme süreleri ölçülerek kısaltıldı (uinput olayları sırayla
  işlenir); `focus`, bir uygulamada işe yaramayan odak yöntemlerini hatırlayıp
  atlar; `type_text` süreç başlatmaz; ekran görüntüsü `spectacle` yerine
  doğrudan KWin'den alınır.
- Çıktı kısadır, Claude daha az okur: girinti seviye başına bir boşluk, aynı
  rol ve eylemli kardeş yapraklar tek satırda
  (`list item ×3: [8] "a" [9] "b" [10] "c"`), Qt'nin hemen her öğeye verdiği
  "Set Focus" eylemi listelenmez. Dolphin + Kate `tree` çıktısı 15.182'den
  7.092 karaktere indi.
- Bir AT-SPI çağrısı en fazla 2 sn bekler; yanıt vermeyen bir uygulama
  sunucuyu kilitlemez. Menü açan eylemler (Qt'de menü kapanana kadar yanıt
  gelmez) 400 ms sonra gönderilmiş sayılır.
- Araç çağrıları aynı anda işlenir: bekleyen bir `wait_for` ya da yavaş bir
  uygulama `ping`'i ve diğer çağrıları bekletmez. Claude bir çağrıyı iptal
  ederse (`notifications/cancelled`) çağrı durdurulur. Klavye ve fare girdisi
  gönderen araçlar yine sırayla çalışır, tuşlar birbirine karışmaz.

## Araçlar

- `apps`, `tree`, `find`, `get_text`: okuma ve arama
- `act`, `set_text`, `focus`, `click`: öğeye işlem. Öğe, `tree`/`find`
  numarasıyla (`id`) ya da tek çağrıda `app` + `query` (+ `role`) ile seçilir:
  adı `query` ile birebir aynı olan ilk görünür öğe, yoksa adında, metninde ya
  da rolünde geçen ilk öğe. `act`, etkin pencere değiştiyse (ör. bir iletişim
  kutusu açıldıysa) bunu bildirir.
- `wait_for`: bir öğe görünene ya da kaybolana kadar bekler (her 50 ms'de bir
  bakar); tuş veya tıklamadan sonra açılan iletişim kutularını beklemek için
- `key`, `type_text`: klavye
- `activate_window`: pencereyi öne getirme
- `mouse`, `screenshot`: yalnızca yedek
- `batch`: birkaç aracı tek çağrıda sırayla çalıştırır, ilk hatada durur;
  Claude'un tur sayısını azaltır

## İzin listesi

Her çağrıda yeniden okunur; ilk çalıştırmada varsayılan liste oluşturulur.

- KDE ve Hyprland: `~/.config/desktop-mcp/allow.txt`, satır başına AT-SPI
  uygulama adı (`kate`, `firefox`…). Hyprland'de erişilebilirliği olmayan
  uygulamalar pencere sınıfıyla (`xterm` gibi) eşleşir.
- Windows: `%APPDATA%\desktop-mcp\allow.txt`, satır başına `.exe` olmadan
  program adı (`notepad`, `firefox`, `chrome`…)

Kurallar:
- Listede olmayan uygulamaların öğeleri okunamaz ve onlara işlem yapılamaz.
- Klavye ve fare girdisi yalnızca öndeki pencere listedeki bir uygulamaya
  aitse gönderilir.
- `activate_window` yalnızca listedeki uygulamaların pencerelerini öne getirir.

Doğru adları görmek için `apps` aracını kullanın.

## Kurulum

Her platformda [Rust](https://rustup.rs/) gerekir. Kurduktan sonra yeni bir
Claude Code oturumu açıp `/mcp` ile `desktop` sunucusunun bağlı olduğunu
kontrol edin.

### KDE Plasma (Wayland)

Gereksinimler:
- KDE Plasma 6, Wayland oturumu
- Sistem paketleri (Arch): `at-spi2-core`, `libkscreen`, `wl-clipboard`,
  `spectacle`
- Hızlı ekran görüntüsü için KWin 6.8 ya da üstü. Daha eski KWin'de
  `screenshot` `spectacle`'a döner; KWin'in `ScreenShot2` arayüzünü açmak için
  bir kez elle bir `.desktop` dosyası kurmak gerekir (ayrıntısı
  `rust/src/linux/kwin.rs` içinde `SHOT_NOT_AUTHORIZED` açıklamasında)
- `/dev/uinput` üzerinde kullanıcı yazma izni (`getfacl /dev/uinput`)

```bash
git clone https://github.com/ahmetadakli/kde-desktop-mcp.git
cd kde-desktop-mcp/rust
cargo build --release
claude mcp add --scope user desktop -- "$PWD/target/release/desktop-mcp"
```

### Hyprland

Gereksinimler:
- Hyprland oturumu (`movecursor` dispatcher'ı)
- Sistem paketleri (Arch): `at-spi2-core`, `wl-clipboard`; önerilen: `wtype`
  (panoya dokunmadan yazma), `grim` (yedek ekran görüntüsü)
- Oturumda AT-SPI veri yolunun çalışması. Qt uygulamaları için Hyprland
  yapılandırmasına `env = QT_LINUX_ACCESSIBILITY_ALWAYS_ON,1` eklemek gerekebilir.
- `/dev/uinput` üzerinde kullanıcı yazma izni

Kurulum komutları KDE ile aynıdır. Claude Code'u Hyprland oturumunun içinden
başlatın ki `HYPRLAND_INSTANCE_SIGNATURE` sunucuya geçsin.

### Windows

Gereksinimler: Windows 10 veya 11. PowerShell'de:

```powershell
git clone https://github.com/ahmetadakli/kde-desktop-mcp.git
cd kde-desktop-mcp\rust
cargo build --release
claude mcp add --scope user desktop -- "$PWD\target\release\desktop-mcp.exe"
```

Linux'ta Windows için derlemek de mümkündür (`mingw-w64-gcc` ve
`rustup target add x86_64-pc-windows-gnu`, sonra
`cargo build --release --target x86_64-pc-windows-gnu`).

## Test

Birim testleri gerçek masaüstü gerektirmez (Windows mantığının saf kısmı
Linux'ta da çalışır):

```bash
cd rust && cargo test --release
```

`tools/mcp_smoke_test.py` gerçek masaüstünde, gerçek MCP protokolü üzerinden
uçtan uca test yapar (Linux, Kate): düzenleyiciyi geçici bir dosyayla açar,
öğeyi bulur, odak başka yere kaydıktan sonra geri odaklar, `batch` ile Türkçe
karakterli metin yazar, art arda hızlı yazmayı ve tuş tekrarını dener, Ctrl+S
ile kaydeder, diskten geri okur ve izin listesinin bilinmeyen bir uygulamayı
engellediğini kontrol eder. Her çağrının süresini yazar. Yalnızca Python
standart kütüphanesi gerekir. Çalışırken klavye ve fareye dokunmayın.

```bash
python3 tools/mcp_smoke_test.py rust/target/release/desktop-mcp
```

`tools/mcp_bench.py` bir ya da daha fazla sunucuyu aynı çağrılarla ölçer ve
medyan ms tablosu yazar (pencere açık olmalı):

```bash
python3 tools/mcp_bench.py --app kate --window notes.txt \
    --server "Rust=rust/target/release/desktop-mcp"
```

## Ölçümler: KDE hızlandırması

`tools/mcp_bench.py`, KDE Plasma 6.8, Kate; 5 turun medyanı (ms). "Önce"
`2f2e5df` (Python'un kaldırıldığı sürüm), "Sonra" bu sürüm:

| İşlem | Önce (ms) | Sonra (ms) | Hızlanma |
|---|---:|---:|---:|
| `initialize` | 4.4 | 3.3 | 1.4x |
| `apps` | 3.8 | 4.2 | 0.9x |
| `activate_window` | 8.5 | 10.3 | 0.8x |
| `tree` | 12.0 | 10.0 | 1.2x |
| `find` | 10.1 | 9.9 | 1.0x |
| `get_text` | 0.3 | 0.3 | 0.9x |
| `key` | 19.5 | 6.4 | 3.0x |
| `type_text` | 34.9 | 2.9 | 12x |
| `focus` | 103.5 | 12.1 | 8.5x |
| `click` | 78.1 | 11.1 | 7.0x |
| `mouse` | 29.3 | 4.9 | 6.0x |
| `screenshot` | 258.8 | 12.1 | 21x |

`activate_window`'daki ~2 ms, `act`'in pencere değişikliğini bildirebilmesi
için etkin pencere başlığının okunmasıdır. Asıl kazanç milisaniyelerin
ötesindedir: kısa çıktı, `app` + `query` ile tek çağrıda hedefleme, `wait_for`
ve `batch` Claude'un tur sayısını ve okuduğu metni azaltır.

## Ölçümler: eski Python sürümüyle karşılaştırma

Sunucu önceden Python ile yazılmıştı; son hali git geçmişinde `4cd2632`
commit'indedir.
Rust sürümü aynı araçları aynı çıktıyla verir: KDE'de 1499, Hyprland'de 2025
satırlık çıktı Python sürümüyle karşılaştırıldı, fark yoktu. Değerler
`tools/mcp_bench.py` ile iki sunucuya aynı çağrı dizisi sırayla uygulanarak
alındı; 5 turun medyanıdır (ms, Kate penceresinde, aynı makine).

KDE Plasma 6 (Wayland):

| İşlem | Python (ms) | Rust (ms) | Hızlanma |
|---|---:|---:|---:|
| `initialize` | 400.5 | 2.2 | 181x |
| `apps` | 48.8 | 4.2 | 11.6x |
| `activate_window` | 22.7 | 7.4 | 3.1x |
| `tree` | 66.6 | 11.5 | 5.8x |
| `find` | 40.8 | 9.4 | 4.3x |
| `get_text` | 5.0 | 0.3 | 15x |
| `key` | 61.3 | 19.4 | 3.2x |
| `type_text` | 197.0 | 35.6 | 5.5x |
| `focus` | 339.2 | 102.8 | 3.3x |
| `click` | 165.5 | 78.4 | 2.1x |
| `mouse` | 66.4 | 28.8 | 2.3x |
| `screenshot` | 269.0 | 254.3 | 1.1x |

Hyprland 0.56 (KDE içinde iç içe çalışan oturum):

| İşlem | Python (ms) | Rust (ms) | Hızlanma |
|---|---:|---:|---:|
| `initialize` | 394.5 | 2.2 | 178x |
| `apps` | 145.6 | 4.3 | 34x |
| `activate_window` | 225.0 | 3.1 | 73x |
| `tree` | 120.9 | 18.5 | 6.5x |
| `find` | 75.9 | 16.1 | 4.7x |
| `get_text` | 4.5 | 0.3 | 13x |
| `key` | 60.5 | 19.2 | 3.1x |
| `type_text` | 28.6 | 11.1 | 2.6x |
| `focus` | 319.5 | 77.7 | 4.1x |
| `click` | 168.7 | 78.0 | 2.2x |
| `mouse` | 69.1 | 28.3 | 2.4x |
| `screenshot` | 38.1 | 24.2 | 1.6x |

Windows (Wine 11.20 içinde Not Defteri; gerçek Windows ölçümü değildir):

| İşlem | Python (ms) | Rust (ms) | Hızlanma |
|---|---:|---:|---:|
| `initialize` | 1998.6 | 62.8 | 32x |
| `apps` | 43.0 | 1.4 | 32x |
| `activate_window` | 5.9 | 0.9 | 6.4x |
| `key` | 54.8 | 19.3 | 2.8x |
| `type_text` | 7.1 | 1.1 | 6.5x |
| `mouse` | 55.6 | 30.5 | 1.8x |
| `screenshot` | 82.3 | 8.3 | 9.9x |

Wine'ın UI Automation istemcisi önbellek isteklerini, ağaç gezmeyi ve çoğu
özelliği desteklemediği için (`E_NOTIMPL`) `tree`, `find`, `get_text`,
`focus` ve `click` Wine'da iki sunucuda da çalışmaz; tablodaki `apps`
ve `activate_window` pencere bulamadan dönen sürelerdir. Girdi adımlarındaki
farkın çoğu Rust'taki daha kısa beklemelerden gelir.

Daha büyük ağaçlarda fark büyür: Dolphin'de gizliler dahil 2000 öğelik `tree`
Python'da 507 ms, Rust'ta 48 ms. `screenshot` süresi çoğunlukla `spectacle`
ve `grim` programlarının kendisidir.

İç içe Hyprland'de uinput'tan gelen Ctrl/Alt'lı kısayollar dış KWin
üzerinden geçerken kaybolur (iki sunucuda da); bu yüzden orada kısayol
denemeleri yapılmadı. Gerçek bir Hyprland oturumunda uinput doğrudan
Hyprland'e gider.

## Bilinen sınırlar

KDE:
- Sunucu erişilebilirlik veri yolunu çalışma zamanında açar. Ondan önce
  başlamış uygulamaların bir kez yeniden başlatılması gerekir.
- Chromium, Chrome ve Electron uygulamaları ağaçlarını yalnızca
  `--force-renderer-accessibility` ile başlatılırlarsa gösterir.
- `type_text`, Qt'nin çok satırlı düzenleyicileri (Kate gibi) dışındaki
  alanlarda panonun içeriğini değiştirir.
- Doğrudan eklemede yazma, yapıştırmadan biraz farklıdır: seçili metni
  değiştirmek iki geri alma adımıdır, çoklu imleçte yalnızca birincil imlece
  yazılır, üzerine yazma kipi ve Kate'in vi kipi atlanır; yapıştırmadaki gibi
  otomatik girinti ve parantez tamamlama çalışmaz.

Hyprland:
- GTK uygulamalarının istemci tarafı gölgeleri, `click` ve `focus` tıklamalarını
  birkaç piksel kaydırabilir.
- Aynı süreçte aynı başlığı taşıyan iki pencere ayırt edilemez.
- `wtype` yoksa `type_text` panoyu değiştirir.

Windows:
- Yönetici olarak çalışan pencerelere, yönetici olmayan bir sunucudan girdi
  gönderilemez (Windows UIPI kuralı).
- Microsoft Store uygulamaları `applicationframehost` adı altında görünebilir.
- Tuş adları ABD klavye düzenindeki konumlara göredir; metin için `type_text`
  kullanın.
- Çok büyük pencerelerde (tarayıcı, Word) `tree` ve `find` bütün alt ağacı
  tek seferde aldığı için yavaşlayabilir.

Hepsi için:
- Oyunlar ve canvas tabanlı arayüzler erişilebilirlik ağacında çoğunlukla boş
  görünür.

## Güvenlik

Sunucu sizin kullanıcı yetkilerinizle çalışır. İzin listesi bir güvenlik
katmanıdır ama işletim sistemi düzeyinde bir yalıtım değildir. Terminalleri,
dosya yöneticisini ve sistem ayarlarını listeye eklemeden önce iki kez
düşünün.

## Lisans

MIT
