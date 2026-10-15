"""Render docs/charts/crates.png from docs/charts/crates.drawio.

A stand-in for `scripts/export-diagrams.sh`, which needs drawio-desktop. It
reads the same source of truth — the .drawio geometry — so the two agree on
layout; the typography is cairo's rather than drawio's.
"""
import cairo, math, re, html
import xml.etree.ElementTree as ET

SCALE = 2
root = ET.parse('docs/charts/crates.drawio').getroot()
model = next(root.iter('mxGraphModel'))
W, H = int(float(model.get('pageWidth'))), int(float(model.get('pageHeight')))
surf = cairo.ImageSurface(cairo.FORMAT_ARGB32, W*SCALE, H*SCALE)
cr = cairo.Context(surf); cr.scale(SCALE, SCALE)
cr.set_source_rgb(1,1,1); cr.paint()

hexrgb = lambda h: tuple(int(h.lstrip('#')[i:i+2],16)/255 for i in (0,2,4))

def rrect(x,y,w,h,r=6):
    cr.new_sub_path()
    cr.arc(x+w-r,y+r,r,-math.pi/2,0); cr.arc(x+w-r,y+h-r,r,0,math.pi/2)
    cr.arc(x+r,y+h-r,r,math.pi/2,math.pi); cr.arc(x+r,y+r,r,math.pi,3*math.pi/2)
    cr.close_path()

def text(s,x,y,size=11,col=(0.33,0.33,0.33),bold=False,italic=False,center=False):
    cr.select_font_face("DejaVu Sans",
        cairo.FONT_SLANT_ITALIC if italic else cairo.FONT_SLANT_NORMAL,
        cairo.FONT_WEIGHT_BOLD if bold else cairo.FONT_WEIGHT_NORMAL)
    cr.set_font_size(size); cr.set_source_rgb(*col)
    ext = cr.text_extents(s)
    cr.move_to(x-(ext.width/2 if center else 0), y); cr.show_text(s)

cells = {c.get('id'): c for c in root.iter('mxCell')}
def geo(c):
    g=c.find('mxGeometry')
    return tuple(float(g.get(k,0)) for k in ('x','y','width','height'))
def sty(c,k):
    m=re.search(rf'{k}=([^;]+)', c.get('style') or ''); return m.group(1) if m else None

for cid,c in cells.items():
    if cid.startswith('band') and not cid.startswith('bandl'):
        x,y,w,h = geo(c); cr.set_source_rgba(*hexrgb(sty(c,'fillColor')),0.35); rrect(x,y,w,h,8); cr.fill()
for cid,c in cells.items():
    if cid.startswith('bandl'):
        x,y,_,_ = geo(c)
        text(re.sub('<[^>]+>','',html.unescape(c.get('value') or '')), x, y+12, 11, (0.54,0.54,0.54), italic=True)

for cid,c in cells.items():
    if c.get('edge') != '1': continue
    sx,sy,sw,sh = geo(cells[c.get('source')]); tx,ty,tw,th = geo(cells[c.get('target')])
    x0,y0,x1,y1 = sx+sw/2, sy+sh, tx+tw/2, ty
    cr.set_source_rgb(*hexrgb('#9aa0a6')); cr.set_line_width(1.2)
    mid=(y0+y1)/2
    cr.move_to(x0,y0); cr.line_to(x0,mid); cr.line_to(x1,mid); cr.line_to(x1,y1-6); cr.stroke()
    cr.move_to(x1,y1); cr.line_to(x1-3.5,y1-6); cr.line_to(x1+3.5,y1-6); cr.close_path(); cr.fill()

for cid,c in cells.items():
    if c.get('vertex')!='1' or cid.startswith('band') or cid in ('title','subtitle'): continue
    x,y,w,h = geo(c); f=sty(c,'fillColor'); s=sty(c,'strokeColor')
    if not f or f=='none': continue
    rrect(x,y,w,h,6); cr.set_source_rgb(*hexrgb(f)); cr.fill_preserve()
    cr.set_source_rgb(*hexrgb(s)) if s and s!='none' else cr.set_source_rgb(1,1,1)
    cr.set_line_width(1.4); cr.stroke()
    lines=[l for l in (re.sub('<[^>]+>','',p).replace('&nbsp;',' ').strip()
                       for p in html.unescape(c.get('value') or '').split('<br>')) if l]
    if not lines: continue
    left = cid=='note'; lh = 16 if left else 15
    top = y+22 if left else y+h/2-(len(lines)-1)*lh/2+4
    for i,l in enumerate(lines):
        bold=(i==0); col=(0.13,0.13,0.13) if bold else (0.33,0.33,0.33)
        size = 12 if (bold and left) else (12.5 if bold else (10.5 if left else 11))
        if left: text(l, x+12, top+i*lh, size, col, bold=bold)
        else:    text(l, x+w/2, top+i*lh, size, col, bold=bold, center=True)

x,y,_,_ = geo(cells['title']); text("kompressor — crates", x, y+26, 23, (0.1,0.1,0.1), bold=True)
x,y,_,_ = geo(cells['subtitle']); sub = html.unescape(cells['subtitle'].get('value'))
# wrap on a word boundary: a fixed character split broke "straig / ht"
cut = sub.rfind(' ', 0, 100)
text(sub[:cut], x, y+15, 12, (0.4,0.4,0.4)); text(sub[cut+1:], x, y+31, 12, (0.4,0.4,0.4))

surf.write_to_png("docs/charts/crates.png")
print(f"wrote docs/charts/crates.png ({W}x{H} at {SCALE}x)")
