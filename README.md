# kde-desktop-mcp

KDE Plasma (Wayland), Hyprland ve Windows için ekran görüntüsü almadan çalışan bir
masaüstü kontrol MCP sunucusu. Claude Code gibi MCP istemcilerine
uygulamaların içini erişilebilirlik ağacından okuma ve öğelere doğrudan işlem
yapma yeteneği verir.

*A screenshot-free desktop control MCP server for KDE Plasma (Wayland), Hyprland
and Windows, built on the platform accessibility APIs, with a per-app allowlist.*

> KDE Plasma gerçek masaüstünde doğrulanmıştır. Hyprland ve Windows desteği
> yenidir ve şimdilik yalnızca birim testleriyle doğrulanmıştır. Sonuçları
> `tools/smoke_test.py` ile paylaşın.

## Nasıl çalışır

Claude ekrana bakmak yerine pencerelerdeki öğeleri `[55] button "Kaydet"` gibi
bir liste olarak okur ve öğelere numarasıyla doğrudan işlem yapar.

| Katman | KDE Plasma (Wayland) | Hyprland | Windows |
|---|---|---|---|
| Okuma | AT-SPI (D-Bus) | AT-SPI (D-Bus) | UI Automation (`uiautomation`) |
| Öğe işlemleri | AT-SPI eylemleri, EditableText | AT-SPI eylemleri, EditableText | Invoke, Toggle, ExpandCollapse, SelectionItem, Value desenleri |
| Klavye | `/dev/uinput` (evdev) | `/dev/uinput` (evdev) | Win32 `SendInput` |
| Fare | Mutlak konumlu uinput | `hyprctl dispatch movecursor` + uinput tuşları | `SetCursorPos` + `SendInput` |
| Metin yazma | `wl-copy` + Ctrl+V (panoyu değiştirir) | `wtype` (panoya dokunmaz); yoksa `wl-copy` | `KEYEVENTF_UNICODE` (panoya dokunmaz) |
| Pencere | KWin betikleri (`qdbus6`) | `hyprctl clients`, `focuswindow` | `SetForegroundWindow` |
| Ekran görüntüsü (yedek) | `spectacle` | `grim` | Pillow `ImageGrab` |

Kod ortak bir araç katmanı (`server.py`) ve platform modüllerinden oluşur:
`backends/linux_common.py` (AT-SPI ve klavye, iki Linux masaüstü için ortak),
`backends/linux_kde.py`, `backends/linux_hyprland.py` ve `backends/windows.py`.
İzin listesi, öğe numaraları ve araçlar ortaktır; Claude her platformda aynı
araçları görür.

Masaüstü otomatik seçilir: Windows'ta Windows modülü, Linux'ta
`HYPRLAND_INSTANCE_SIGNATURE` tanımlıysa Hyprland, değilse KDE. Elle seçmek
için `DESKTOP_MCP_BACKEND` ortam değişkenine `kde`, `hyprland` ya da `windows`
verin.

Hyprland'de etkin pencere, AT-SPI'deki uygulamayla süreç kimliği (pid)
üzerinden eşleştirilir; böylece araç takımının odak bayrağına güvenmek
gerekmez.

`focus` önce erişilebilirlik altyapısından odak ister. Uygulama bunu reddederse
(Qt'de sık görülür) öğenin ortasına gerçek bir tıklama yapar.

## Araçlar

- `apps`, `tree`, `find`, `get_text`: okuma ve arama
- `act`, `set_text`, `focus`, `click`: öğeye işlem
- `key`, `type_text`: klavye
- `activate_window`: pencereyi öne getirme
- `mouse`, `screenshot`: yalnızca yedek
- `batch` (yalnızca Rust sunucusu): birkaç aracı tek çağrıda sırayla çalıştırır,
  ilk hatada durur; Claude'un tur sayısını azaltır

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

## Kurulum: KDE Plasma (Wayland)

Gereksinimler:
- KDE Plasma 6, Wayland oturumu
- Sistem paketleri (Arch): `at-spi2-core`, `python-gobject`, `qt6-tools`,
  `libkscreen`, `wl-clipboard`, `spectacle`
- `/dev/uinput` üzerinde kullanıcı yazma izni (`getfacl /dev/uinput`)
- Python 3.12+ ve [uv](https://docs.astral.sh/uv/)

```bash
git clone https://github.com/ahmetadakli/kde-desktop-mcp.git
cd kde-desktop-mcp
uv venv --python /usr/bin/python3 --system-site-packages .venv
uv pip install --python .venv/bin/python -r pyproject.toml
claude mcp add --scope user desktop -- "$PWD/.venv/bin/python" "$PWD/server.py"
```

`--system-site-packages` gereklidir: PyGObject ve AT-SPI PyPI'dan değil,
sistem paketlerinden gelir.

## Hızlı sunucu: Rust

`rust/` altında aynı araçları aynı çıktı biçimiyle sunan bir Rust sunucusu
var (KDE Plasma ve Hyprland'de Python sunucusuyla satır satır karşılaştırıldı:
fark yok). Ayrıca birkaç aracı tek çağrıda çalıştıran `batch` aracı vardır.

Neden hızlı:
- AT-SPI'ye libatspi yerine doğrudan D-Bus ile konuşur ve istekleri
  beklemeden art arda gönderir (pipelining): bir öğenin bütün öznitelikleri ve
  ağacın her seviyesi aynı anda istenir; süre gidiş-dönüş gecikmesiyle değil
  uygulamanın yanıt hızıyla sınırlanır.
- KWin betikleri `qdbus6` süreçleri yerine D-Bus'tan, Hyprland komutları
  `hyprctl` süreçleri yerine doğrudan Hyprland'in soketinden gider.
- Açılış 2 ms (Python ~390 ms); sanal klavye ve fare açılışta hazırlanır.

### Ölçümler

`tools/mcp_bench.py` ile, iki sunucuya aynı çağrı dizisi sırayla uygulandı;
değerler 5 turun medyanıdır (ms, Kate penceresinde, aynı makine).

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
ve `activate_window` pencere bulamadan dönen sürelerdir. Rust'ın Windows
okuma kodu (bütün alt ağacı tek `BuildUpdatedCache` çağrısıyla alan UIA
önbellek istekleri) derlenir ve birim testleri geçer ama henüz gerçek
Windows'ta denenmemiştir. Girdi adımlarındaki farkın çoğu Rust'taki daha
kısa beklemelerden gelir.

Daha büyük ağaçlarda fark büyür: Dolphin'de gizliler dahil 2000 öğelik `tree`
Python'da 507 ms, Rust'ta 48 ms. `screenshot` süresi çoğunlukla `spectacle`
ve `grim` programlarının kendisidir.

İç içe Hyprland'de uinput'tan gelen Ctrl/Alt'lı kısayollar dış KWin
üzerinden geçerken kaybolur (iki sunucuda da); bu yüzden orada kısayol
denemeleri yapılmadı. Gerçek bir Hyprland oturumunda uinput doğrudan
Hyprland'e gider.

### Kurulum

Gereksinimler platformun kendi kurulumuyla aynıdır (PyGObject ve uv
gerekmez), artı [Rust](https://rustup.rs/):

```bash
cd kde-desktop-mcp/rust
cargo build --release
claude mcp add --scope user desktop -- "$PWD/target/release/desktop-mcp"
```

Windows'ta (PowerShell):

```powershell
cd kde-desktop-mcp\rust
cargo build --release
claude mcp add --scope user desktop -- "$PWD\target\release\desktop-mcp.exe"
```

Linux'ta Windows için derlemek de mümkündür (`mingw-w64-gcc` ve
`rustup target add x86_64-pc-windows-gnu`, sonra
`cargo build --release --target x86_64-pc-windows-gnu`).

Masaüstü Python sunucusundaki gibi seçilir (`HYPRLAND_INSTANCE_SIGNATURE`
varsa Hyprland, yoksa KDE; `DESKTOP_MCP_BACKEND` ile elle).

## Kurulum: Hyprland

Gereksinimler:
- Hyprland oturumu (`hyprctl` ve `movecursor` dispatcher'ı)
- Sistem paketleri (Arch): `at-spi2-core`, `python-gobject`, `wl-clipboard`;
  önerilen: `wtype` (panoya dokunmadan yazma), `grim` (yedek ekran görüntüsü)
- Oturumda AT-SPI veri yolunun çalışması. Qt uygulamaları için Hyprland
  yapılandırmasına `env = QT_LINUX_ACCESSIBILITY_ALWAYS_ON,1` eklemek gerekebilir.
- `/dev/uinput` üzerinde kullanıcı yazma izni
- Python 3.12+ ve [uv](https://docs.astral.sh/uv/)

Kurulum komutları KDE ile aynıdır. Claude Code'u Hyprland oturumunun içinden
başlatın ki `HYPRLAND_INSTANCE_SIGNATURE` sunucuya geçsin.

## Kurulum: Windows

Gereksinimler: Windows 10 veya 11, Python 3.12+ ve
[uv](https://docs.astral.sh/uv/). PowerShell'de:

```powershell
git clone https://github.com/ahmetadakli/kde-desktop-mcp.git
cd kde-desktop-mcp
uv venv .venv
uv pip install --python .venv\Scripts\python.exe -r pyproject.toml
claude mcp add --scope user desktop -- "$PWD\.venv\Scripts\python.exe" "$PWD\server.py"
```

Her iki platformda da yeni bir Claude Code oturumu açıp `/mcp` ile `desktop`
sunucusunun bağlı olduğunu kontrol edin.

## Test

Birim testleri gerçek masaüstü gerektirmez (Hyprland ve Windows mantığı sahte
`hyprctl` çıktıları ve sahte UI Automation nesneleriyle denenir):

```bash
.venv/bin/python -m unittest discover -s tests
```

`tools/smoke_test.py` gerçek masaüstünde uçtan uca test yapar: düzenleyiciyi
(KDE ve Hyprland'de Kate, Windows'ta Not Defteri) geçici bir dosyayla açar, öğeyi bulur,
odak başka yere kaydıktan sonra geri odaklar, Türkçe karakterli metin yazar,
Ctrl+S ile kaydeder, diskten geri okur ve izin listesinin bilinmeyen bir
uygulamayı engellediğini kontrol eder. Çalışırken klavye ve fareye dokunmayın.

```bash
.venv/bin/python tools/smoke_test.py
```

```powershell
.venv\Scripts\python.exe tools\smoke_test.py
```

`tools/mcp_smoke_test.py` aynı denemeyi gerçek MCP protokolü üzerinden
herhangi bir sunucuya karşı yapar (Linux, Kate); ayrıca art arda hızlı yazmayı,
tuş tekrarını, `batch` ve `click` araçlarını dener ve her çağrının süresini
yazar:

```bash
(cd rust && cargo test --release)
.venv/bin/python tools/mcp_smoke_test.py rust/target/release/desktop-mcp
```

`tools/mcp_bench.py` iki sunucuyu aynı çağrılarla ölçer ve medyan ms
tablosu yazar (pencere açık olmalı):

```bash
.venv/bin/python tools/mcp_bench.py --app kate --window notes.txt \
    --server "Python=.venv/bin/python server.py" \
    --server "Rust=rust/target/release/desktop-mcp"
```

## Bilinen sınırlar

KDE:
- Sunucu erişilebilirlik veri yolunu çalışma zamanında açar. Ondan önce
  başlamış uygulamaların bir kez yeniden başlatılması gerekir.
- Chromium, Chrome ve Electron uygulamaları ağaçlarını yalnızca
  `--force-renderer-accessibility` ile başlatılırlarsa gösterir.
- `type_text` panonun içeriğini değiştirir.

Hyprland:
- GTK uygulamalarının istemci tarafı gölgeleri, `click` ve `focus` tıklamalarını
  birkaç piksel kaydırabilir.
- `wtype` yoksa `type_text` panoyu değiştirir.

Windows:
- Yönetici olarak çalışan pencerelere, yönetici olmayan bir sunucudan girdi
  gönderilemez (Windows UIPI kuralı).
- Microsoft Store uygulamaları `applicationframehost` adı altında görünebilir.
- Tuş adları ABD klavye düzenindeki konumlara göredir; metin için `type_text`
  kullanın.

İkisi için:
- Oyunlar ve canvas tabanlı arayüzler erişilebilirlik ağacında çoğunlukla boş
  görünür.

## Güvenlik

Sunucu sizin kullanıcı yetkilerinizle çalışır. İzin listesi bir güvenlik
katmanıdır ama işletim sistemi düzeyinde bir yalıtım değildir. Terminalleri,
dosya yöneticisini ve sistem ayarlarını listeye eklemeden önce iki kez
düşünün.

## Lisans

MIT
