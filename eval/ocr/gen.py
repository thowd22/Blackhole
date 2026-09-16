# Synthetic OCR benchmark images with known ground truth (no personal data; gitignored anyway).
from PIL import Image, ImageDraw, ImageFont
import json, glob
fonts = {f.split('/')[-1]: f for f in glob.glob('/usr/share/fonts/truetype/**/*.ttf', recursive=True)}
def font(name_part, size):
    for k, v in fonts.items():
        if name_part.lower() in k.lower(): return ImageFont.truetype(v, size)
    return ImageFont.load_default()
cases = []
def make(name, lines, fnt, size, bg, fg, w=900, pad=24, spacing=8):
    img = Image.new('RGB', (w, pad*2 + len(lines)*(size+spacing)), bg)
    d = ImageDraw.Draw(img)
    for i, l in enumerate(lines):
        d.text((pad, pad + i*(size+spacing)), l, font=font(fnt, size), fill=fg)
    img.save(f'{name}.png'); cases.append({'file': f'{name}.png', 'text': '\n'.join(lines)})
make('terminal', ['$ cargo build --release --target x86_64-pc-windows-gnu', '   Compiling blackhole v0.2.0 (/home/admin2/blackhole)', '    Finished release profile [optimized] in 51.03s', 'warning: 2 unused imports in src/search.rs'], 'DejaVuSansMono', 18, (24,10,24), (240,232,255))
make('paragraph', ['The espresso machine warranty expires on 2027-02-01.', 'Serial number 44-1188, purchased from Bezzera Direct for USD 1,245.00.', 'Keep the receipt in the kitchen drawer with the manual.'], 'DejaVuSans', 22, (255,255,255), (20,20,20))
make('table', ['Item            Qty    Price     Total', 'Espresso beans    2    18.50     37.00', 'Filter papers     1     6.25      6.25', 'Descaler          3     9.90     29.70'], 'DejaVuSansMono', 20, (250,248,240), (30,30,30))
make('small_ui', ['File  Edit  View  Terminal  Help', 'Settings > Appearance > Theme: Dark violet', 'Notifications are enabled for this workspace', 'Last synced 3 minutes ago'], 'DejaVuSans', 14, (43,43,43), (220,220,220))
make('chat', ['Alice: can you send me the customs entry number?', 'Bob: sure, it is AN9-0100789-6, arrived 11/04/2025', 'Alice: thanks! the broker was Joseph B Hohenstein', 'Bob: right, in Savannah GA'], 'DejaVuSans', 20, (255,255,255), (40,40,40))
make('bold_serif', ['Quarterly Report 2026', 'Revenue grew 12.4% year over year', 'Operating margin: 18.9 percent'], 'DejaVuSerif-Bold', 26, (255,255,255), (0,0,0))
json.dump(cases, open('truth.json','w'), indent=1)
print(len(cases), 'images')
