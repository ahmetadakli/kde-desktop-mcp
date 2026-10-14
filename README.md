# kde-desktop-mcp

KDE Plasma (Wayland) ve Windows için ekran görüntüsü almadan çalışan bir
masaüstü kontrol MCP sunucusu. Claude Code gibi MCP istemcilerine
uygulamaların içini erişilebilirlik ağacından okuma ve öğelere doğrudan işlem
yapma yeteneği verir.

*A screenshot-free desktop control MCP server for KDE Plasma on Wayland and
Windows, built on the platform accessibility APIs, with a per-app allowlist.*

> Windows desteği yenidir ve henüz gerçek bir Windows makinesinde
> doğrulanmamıştır. Sonuçları `tools/smoke_test.py` ile paylaşın.

## Nasıl çalışır

Claude ekrana bakmak yerine pencerelerdeki öğeleri `[55] button "Kaydet"` gibi
bir liste olarak okur ve öğelere numarasıyla doğrudan işlem yapar.

| Katman | KDE Plasma (Wayland) | Windows |
|---|---|---|
| Okuma | AT-SPI (D-Bus) | UI Automation (`uiautomation`) |
| Öğe işlemleri | AT-SPI eylemleri, EditableText | Invoke, Toggle, ExpandCollapse, SelectionItem, Value desenleri |
| Klavye ve fare | `/dev/uinput` sanal aygıtları (evdev) | Win32 `SendInput`, `SetCursorPos` |
| Metin yazma | `wl-copy` + Ctrl+V (panoyu değiştirir) | `KEYEVENTF_UNICODE` (panoya dokunmaz) |
| Pencere | KWin betikleri (`qdbus6`) | `SetForegroundWindow` |
| Ekran görüntüsü (yedek) | `spectacle` | Pillow `ImageGrab` |

Kod ortak bir araç katmanı (`server.py`) ve platform modüllerinden
(`backends/linux_kde.py`, `backends/windows.py`) oluşur. İzin listesi, öğe
numaraları ve araçlar ortaktır; Claude iki platformda aynı araçları görür.

`focus` önce erişilebilirlik altyapısından odak ister. Uygulama bunu reddederse
(Qt'de sık görülür) öğenin ortasına gerçek bir tıklama yapar.

## Araçlar

- `apps`, `tree`, `find`, `get_text`: okuma ve arama
- `act`, `set_text`, `focus`, `click`: öğeye işlem
- `key`, `type_text`: klavye
- `activate_window`: pencereyi öne getirme
- `mouse`, `screenshot`: yalnızca yedek

## İzin listesi

Her çağrıda yeniden okunur; ilk çalıştırmada varsayılan liste oluşturulur.

- KDE: `~/.config/desktop-mcp/allow.txt`, satır başına AT-SPI uygulama adı
  (`kate`, `firefox`…)
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

`tools/smoke_test.py` gerçek masaüstünde uçtan uca test yapar: düzenleyiciyi
(KDE'de Kate, Windows'ta Not Defteri) geçici bir dosyayla açar, öğeyi bulur,
odak başka yere kaydıktan sonra geri odaklar, Türkçe karakterli metin yazar,
Ctrl+S ile kaydeder, diskten geri okur ve izin listesinin bilinmeyen bir
uygulamayı engellediğini kontrol eder. Çalışırken klavye ve fareye dokunmayın.

```bash
.venv/bin/python tools/smoke_test.py
```

```powershell
.venv\Scripts\python.exe tools\smoke_test.py
```

## Bilinen sınırlar

KDE:
- Sunucu erişilebilirlik veri yolunu çalışma zamanında açar. Ondan önce
  başlamış uygulamaların bir kez yeniden başlatılması gerekir.
- Chromium, Chrome ve Electron uygulamaları ağaçlarını yalnızca
  `--force-renderer-accessibility` ile başlatılırlarsa gösterir.
- `type_text` panonun içeriğini değiştirir.

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
