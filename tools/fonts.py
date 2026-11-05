"""裁界面字体（assets/fonts/）：只留源码字符串里用到的非 ASCII 字 + ASCII + Latin-1 + 常用标点和名字常用字。
界面文字改了（新加了中文）就重跑一次；没裁进去的字程序里会退回微软雅黑画（ui.rs），不会出方块。

用法：python -P tools/fonts.py <源字体目录>
  源字体目录里要有：ResourceHanRoundedCN/ResourceHanRoundedCN-{Regular,Bold}.ttf（源樣黑體圓 v0.990 静态 TTF，
  https://github.com/CyanoHao/Resource-Han-Rounded/releases，OFL）、Nunito/Nunito-VF.ttf（google/fonts，OFL）。
源樣黑體圓的名字里带保留字体名 "Source"，按 OFL 改过的版本要换名：裁出来的叫 "Km Rounded"。
Nunito 切成两个静态字重，GDI 里是独立家族 "Nunito SemiBold" / "Nunito ExtraBold"（按 weight 400 建）。"""
import sys, os, re, glob
from fontTools import subset
from fontTools.ttLib import TTFont
from fontTools.varLib import instancer
sys.stdout.reconfigure(encoding='utf-8')
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC = os.path.abspath(sys.argv[1])
OUT = os.path.join(ROOT, 'assets', 'fonts')
os.makedirs(OUT, exist_ok=True)

chars = set()
for f in glob.glob(os.path.join(ROOT, 'src', '*.rs')):
    for m in re.finditer(r'"((?:[^"\\n]|\.)*)"', open(f, encoding='utf-8').read()):
        chars |= {c for c in m.group(1) if ord(c) > 127}
chars |= {chr(c) for c in range(0x20, 0x7f)} | {chr(c) for c in range(0xa0, 0x100)}
chars |= set('–—‘’“”…·•×÷±°%‰、。，：；？！（）【】《》「」～')
# 电脑名里常见的字（名字是用户填的，没裁进去的退回雅黑）
chars |= set('电脑台式机笔记本客厅书房卧室办公室家公司主副左右上下前后工位游戏学习工作小大新旧白黑红蓝绿的我你他她妈爸哥姐弟妹')
latin = {chr(c) for c in range(0x20, 0x7f)} | {chr(c) for c in range(0xa0, 0x100)} | set('–—‘’“”…·•×')
print(len(chars), 'chars')

def opts():
    o = subset.Options(); o.layout_features = ['*']; o.name_IDs = ['*']; o.name_languages = ['*']
    o.notdef_outline = True; o.glyph_names = False; o.hinting = True
    return o

def cut(f, keep, dst):
    s = subset.Subsetter(opts()); s.populate(unicodes=[ord(c) for c in keep]); s.subset(f); f.save(dst)
    print(os.path.basename(dst), f'{os.path.getsize(dst) / 1e3:.0f} KB')

for style in ('Regular', 'Bold'):
    f = TTFont(os.path.join(SRC, 'ResourceHanRoundedCN', f'ResourceHanRoundedCN-{style}.ttf'))
    for r in f['name'].names:
        if r.nameID in (1, 16):
            r.string = 'Km Rounded'
        elif r.nameID == 4:
            r.string = f'Km Rounded {style}'
        elif r.nameID == 6:
            r.string = f'KmRounded-{style}'
    cut(f, chars, os.path.join(OUT, f'KmRounded-{style}.ttf'))

for wt, name in ((600, 'SemiBold'), (800, 'ExtraBold')):
    vf = TTFont(os.path.join(SRC, 'Nunito', 'Nunito-VF.ttf'))
    st = instancer.instantiateVariableFont(vf, {'wght': wt}, updateFontNames=False)
    for r in st['name'].names:
        if r.nameID in (1, 16):
            r.string = f'Nunito {name}'
        elif r.nameID in (2, 17):
            r.string = 'Regular'
        elif r.nameID == 4:
            r.string = f'Nunito {name}'
        elif r.nameID == 6:
            r.string = f'Nunito-{name}'
    st['OS/2'].usWeightClass = 400
    st['OS/2'].fsSelection = (st['OS/2'].fsSelection & ~0x21) | 0x40
    st['head'].macStyle &= ~1
    cut(st, latin, os.path.join(OUT, f'Nunito-{name}.ttf'))
