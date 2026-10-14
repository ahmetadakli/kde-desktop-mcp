# kde-desktop-mcp

KDE Plasma (Wayland) için ekran görüntüsü almadan çalışan bir masaüstü kontrol
MCP sunucusu. Claude Code gibi MCP istemcilerine uygulamaların içini
erişilebilirlik ağacından (AT-SPI) okuma ve öğelere doğrudan işlem yapma
yeteneği verir.

*A screenshot-free desktop control MCP server for KDE Plasma on Wayland, built on
AT-SPI, uinput and KWin scripting, with a per-app allowlist.*

## Nasıl çalışır

| Katman | Teknik | Ne yapar |
|---|---|---|
| Okuma | AT-SPI (D-Bus) | Pencerelerdeki öğeleri `[55] button "Kaydet"` gibi listeler |
| İşlem | AT-SPI eylemleri | Düğmeye basma, metin okuma/yazma; fare hareket etmez |
| Girdi | `/dev/uinput` (evdev) | Sanal klavye ve mutlak konumlu fare |
| Yazma | `wl-copy` + Ctrl+V | Her klavye düzeninde ve Unicode ile çalışır |
| Pencere | KWin betikleri (`qdbus6`) | Pencereyi öne getirme, aktif pencerenin konumunu okuma |

Qt uygulamaları dışarıdan odak isteğini çoğu zaman reddeder. `focus` bu yüzden
sırayla AT-SPI odak isteğini, "Set Focus" eylemini ve son olarak öğenin
ortasına gerçek bir tıklamayı dener. Wayland'da öğe konumları pencereye göre
geldiği için pencerenin ekrandaki yeri KWin'den alınır.

## Araçlar

- `apps`, `tree`, `find`, `get_text`: okuma ve arama
- `act`, `set_text`, `focus`, `click`: öğeye işlem
- `key`, `type_text`: klavye
- `activate_window`: pencereyi öne getirme
- `mouse`, `screenshot`: yalnızca yedek

## İzin listesi

`~/.config/desktop-mcp/allow.txt` her çağrıda yeniden okunur (ilk çalıştırmada
varsayılan liste oluşturulur). Satır başına bir AT-SPI uygulama adı yazılır.

- Listede olmayan uygulamaların öğeleri okunamaz ve onlara işlem yapılamaz.
- Klavye ve fare girdisi yalnızca aktif pencere listedeki bir uygulamaya aitse
  gönderilir.
- `activate_window` yalnızca listedeki uygulamaların pencerelerini öne getirir.

## Gereksinimler

- KDE Plasma 6, Wayland oturumu
- Sistem paketleri: `at-spi2-core`, `python-gobject`, `qt6-tools` (`qdbus6`),
  `libkscreen` (`kscreen-doctor`), `wl-clipboard`, `spectacle`
- `/dev/uinput` üzerinde kullanıcı yazma izni (`getfacl /dev/uinput` ile kontrol edin)
- Python 3.12+ ve [uv](https://docs.astral.sh/uv/)

## Kurulum

```bash
git clone git@github.com:ahmetadakli/kde-desktop-mcp.git
cd kde-desktop-mcp
uv venv --python /usr/bin/python3 --system-site-packages .venv
uv pip install --python .venv/bin/python -r pyproject.toml
claude mcp add --scope user desktop -- "$PWD/.venv/bin/python" "$PWD/server.py"
```

`--system-site-packages` gereklidir: PyGObject ve AT-SPI PyPI'dan değil, sistem
paketlerinden gelir.

Yeni bir Claude Code oturumu açıp `/mcp` ile `desktop` sunucusunun bağlı
olduğunu kontrol edin.

## Bilinen sınırlar

- Sunucu erişilebilirlik veri yolunu çalışma zamanında açar. Ondan önce
  başlamış uygulamaların bir kez yeniden başlatılması gerekir.
- Chromium, Chrome ve Electron uygulamaları ağaçlarını yalnızca
  `--force-renderer-accessibility` ile başlatılırlarsa gösterir.
- Oyunlar ve canvas tabanlı arayüzler AT-SPI'de çoğunlukla boş görünür.
- `type_text` panonun içeriğini değiştirir.
- Yalnızca tek bir KDE Wayland makinesinde denenmiştir.

## Güvenlik

Sunucu sizin kullanıcı yetkilerinizle çalışır. İzin listesi bir güvenlik
katmanıdır ama işletim sistemi düzeyinde bir yalıtım değildir. Terminalleri ve
sistem ayarlarını listeye eklemeden önce iki kez düşünün.

## Lisans

MIT
