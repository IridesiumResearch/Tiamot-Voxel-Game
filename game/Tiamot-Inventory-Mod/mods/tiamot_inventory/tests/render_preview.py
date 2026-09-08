# SPDX-FileCopyrightText: Iridesium
# SPDX-License-Identifier: GPL-3.0-only
"""Render the real Lua layout with representative stacks, not a game screenshot.
Requires Pillow, blake3 and lupa; run from any directory.
"""
from pathlib import Path
from PIL import Image, ImageDraw, ImageFont
from lupa.lua54 import LuaRuntime
ROOT=Path(__file__).resolve().parents[3]
lua=LuaRuntime(unpack_returned_tuples=True)
lua.execute('''
game={OCCUPANCY_FULL=0x7FFFFFF,ITEMS_PER_STACK=90}
function game.register_hud_script() end
function game.register_action() end
function game.register_sound() end
function game.bind_sound() end
function game.register_on_action(f) action=f end
function game.register_on_dialog_event(f) event=f end
function game.register_on_player_leave() end
function game.inventory() return {{material=31,units=124},{material=45,units=270}} end
function game.block_of(id) return id==31 and "core:granite" or "core:limestone" end
function game.show_dialog(s) tree=s.tree return true end
game.update_dialog=game.show_dialog
''')
lua.execute((ROOT/'mods/tiamot_inventory/init.lua').read_text())
def convert(v):
    if hasattr(v,'items'):
        vals=dict(v.items())
        if vals and all(isinstance(k,int) for k in vals): return [convert(vals[k]) for k in sorted(vals)]
        return {k:convert(val) for k,val in vals.items()}
    return v
lua.execute('action{player="preview", id="tiamot_inventory:inventory",pressed=true}')
inventory=convert(lua.globals().tree)
lua.execute('event{player="preview",form="tiamot_inventory:inventory",kind="pressed",name="shapes"};event{player="preview",form="tiamot_inventory:inventory",kind="pressed",name="stairs"}')
crafter=convert(lua.globals().tree)
W,H=1800,1220
im=Image.new('RGB',(W,H),'#101417'); d=ImageDraw.Draw(im)
font_path=str(ROOT/'crates/client/assets/third-party/cinzel-decorative/CinzelDecorative-Bold.ttf')
def font(s): return ImageFont.truetype(font_path,max(8,round(s)))
def colour(c, fallback): return tuple(c[:3]) if c else fallback

def natural(n):
    t=n['type']; st=n.get('style') or {}; sz=st.get('text_size',14)
    if t=='container':
        ns=[natural(c) for c in n.get('children',[])]; row=n.get('direction')=='row'; p=n.get('padding',0); gap=n.get('gap',0)
        mains=[c.get('size',v[0 if row else 1]) for c,v in zip(n.get('children',[]),ns)]
        cross=[c.get('cross_size',v[1 if row else 0]) for c,v in zip(n.get('children',[]),ns)]
        a=sum(mains)+max(0,len(ns)-1)*gap+2*p; b=max(cross,default=0)+2*p
        return (a,b) if row else (b,a)
    if t=='item_slot': return 36,36
    if t=='shape_editor': return 192,192
    text=n.get('text','')
    if t=='dropdown': text=n['options'][n['selected']-1]
    width=d.textlength(text,font=font(sz)); height=sz*1.2
    return (width+(32 if t=='dropdown' else (40 if st.get('nine_slice') else 16) if t=='button' else 0),height+(8 if t in ('button','dropdown') else 0))

def cube(cx,cy,size,base=(115,119,116)):
    dx=size*.48; dy=size*.24; h=size*.53
    top=[(cx,cy-dy),(cx+dx,cy),(cx,cy+dy),(cx-dx,cy)]
    left=[top[3],top[2],(cx,cy+dy+h),(cx-dx,cy+h)]
    right=[top[2],top[1],(cx+dx,cy+h),(cx,cy+dy+h)]
    for pts,m in [(left,.68),(right,.86),(top,1.16)]:
        d.polygon(pts,fill=tuple(min(255,int(v*m)) for v in base),outline='#252c2e')

def shape(x,y,w,h,mask):
    s=min(w*.18,h*.25); cx=x+w/2; cy=y+h*.32
    for z in range(2,-1,-1):
        for yy in range(3):
            for xx in range(3):
                if mask & (1<<(xx+3*yy+9*z)):
                    cube(cx+(xx-z)*s*.49,cy+(xx+z)*s*.245-yy*s*.53,s)

textures={}
import blake3
for path in (ROOT/'mods/tiamot_inventory/textures').glob('*.png'):
    textures[blake3.blake3(b'tiamot:content:v1' + path.read_bytes()).hexdigest()]=Image.open(path).convert('RGB')
def frame(hash,x,y,w,h,sliced=True):
    key=bytes(hash).hex() if isinstance(hash,list) else hash
    source=textures[key]; x,y,w,h=map(round,(x,y,w,h))
    if not sliced:
        im.paste(source.resize((max(1,w),max(1,h)),Image.Resampling.LANCZOS),(x,y));return
    edge=min(52,w/4,h/4); xs=[0,edge,w-edge,w]; ys=[0,edge,h-edge,h]; uv=[0,.25,.75,1]
    for cy in range(3):
        for cx in range(3):
            a,b,c,e=map(round,(xs[cx],ys[cy],xs[cx+1],ys[cy+1]))
            if c<=a or e<=b:continue
            crop=source.crop(tuple(round(v) for v in (uv[cx]*source.width,uv[cy]*source.height,uv[cx+1]*source.width,uv[cy+1]*source.height)))
            im.paste(crop.resize((c-a,e-b),Image.Resampling.LANCZOS),(x+a,y+b))

def paint(n, x,y,w,h):
    st=n.get('style') or {}; t=n['type']; rect=(round(x),round(y),round(x+w),round(y+h))
    if st.get('background'): d.rounded_rectangle(rect, radius=3,fill=colour(st['background'],'#161a1d'))
    if st.get('border'): d.rounded_rectangle(rect,radius=3,outline=colour(st['border'],'#5b605f'),width=1)
    if st.get('nine_slice'):frame(st['nine_slice'],x,y,w,h)
    if t=='container':
        row=n.get('direction')=='row'; p=n.get('padding',0); gap=n.get('gap',0); kids=n.get('children',[])
        main=(w if row else h)-2*p; cross=(h if row else w)-2*p
        ns=[natural(c) for c in kids]; sizes=[c.get('size',v[0 if row else 1]) for c,v in zip(kids,ns)]
        used=sum(sizes); avail=main-max(0,len(kids)-1)*gap; grow=sum(c.get('grow',0) for c in kids)
        if used>avail and used: sizes=[v*max(0,avail)/used for v in sizes]
        elif grow: sizes=[v+(avail-used)*c.get('grow',0)/grow for c,v in zip(kids,sizes)]
        cur=0
        for c,v,nn in zip(kids,sizes,ns):
            cs=c.get('cross_size',cross if n.get('align')=='stretch' else min(cross,nn[1 if row else 0]))
            off=(cross-cs)/2 if n.get("align")=="center" else 0
            paint(c,x+p+(cur if row else off),y+p+(off if row else cur),v if row else cs,cs if row else v)
            cur+=v+gap
    elif t in ('label','button','dropdown'):
        text=n.get('text','') if t!='dropdown' else n['options'][n['selected']-1]
        sz=st.get('text_size',14); f=font(sz); tw=d.textlength(text,font=f)
        tx=x+(w-tw)/2 if t=='button' else x+(16 if t=='dropdown' else 0)
        d.text((tx,y+(h-sz)/2-2),text,font=f,fill=colour(st.get('text_colour'),'#e0d8c4'))
    elif t=='item_slot':
        index=n['index']
        if index in (1,2,3,6,10,11,14,20,23,28):
            if index in (3,14): shape(x+6,y+3,w-12,h-12,0x7)
            else: cube(x+w/2,y+h*.31,min(w,h)*.49, (158,147,121) if index%2 else (107,121,122))
            q={1:'4+16',2:'10',3:'8',28:'1'}.get(index,str(index+2))
            d.text((x+w-4-d.textlength(q,font=font(15)),y+h-21),q,font=font(15),fill='#ebdfc4')
    elif t=='shape_editor':
        shape(x,y,w,h,n['shape'])
        for xx,txt in [(x+10,'<'),(x+w-44,'>')]:
            d.rounded_rectangle((xx,y+10,xx+34,y+44),radius=3,fill='#404344')
            d.text((xx+11,y+14),txt,font=font(17),fill='#ded6c3')

for x,tree,title in [(48,inventory,'01  /  INVENTORY'),(936,crafter,'02  /  SHAPE CRAFTER')]:
    d.text((x,38),title,font=font(14),fill='#ad956b')
    paint(tree,x,80,816,880)
# Capture the mod's real HUD commands.
lua.execute('hud={};commands={};function hud.on_draw(f) draw=f end;function hud.rect(c) c.kind="rect";table.insert(commands,c) end;function hud.text(c) c.kind="text";table.insert(commands,c) end;function hud.icon(c) c.kind="icon";table.insert(commands,c) end;function hud.image(c) c.kind="image";table.insert(commands,c) end')
lua.execute((ROOT/'mods/tiamot_inventory/hud.lua').read_text())
lua.execute('draw{selected=3, carried={[1]={material=31,blocks=4,nodes=16},[2]={material=45,blocks=10,nodes=0},[3]={material=31,shape=7,count=8}},offhand={material=45,blocks=1,nodes=0},tool={name="Chisel"},looking_at={name="Granite"}}')
d.text((48,988),'03  /  QUICK ACCESS HUD',font=font(14),fill='#ad956b')
for c in convert(lua.globals().commands):
    x=W/2+c['x']; y=1176-c['y']; co=colour(c.get('colour'),'#ddd4bf')
    if c['kind']=='rect': d.rectangle((x,y,x+c['w']-1,y+c['h']-1),fill=co)
    elif c['kind']=='text': d.text((x,y),c['text'],font=font(c['size']),fill=co)
    elif c['kind']=='image':frame(c['hash'],x,y,c['w'],c['h'],False)
    elif c['kind']=='icon':
        if c.get('shape'): shape(x,y,c['size'],c['size'],c['shape'])
        else: cube(x+c['size']/2,y+c['size']*.25,c['size']*.7)
d.text((48,1196),'LUA LAYOUT PREVIEW  /  Representative items  /  In-game appearance and interaction pending verification',font=font(12),fill='#82928e')
p=ROOT/'mods/tiamot_inventory/preview.png'; im.save(p); print(p)
