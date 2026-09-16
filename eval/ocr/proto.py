# Reference implementation of the PP-OCR pipeline (numpy + onnxruntime CPU), used to
# validate the Rust port and to pick the recognizer. Word accuracy against truth.json.
import numpy as np, onnxruntime as ort, json, sys, time
from PIL import Image
from scipy import ndimage
M='/home/admin2/blackhole/models/ocr/'
det = ort.InferenceSession(M+'det.onnx', providers=['CPUExecutionProvider'])
recs = {'en': (ort.InferenceSession(M+'rec_en.onnx', providers=['CPUExecutionProvider']), [l.rstrip('\n') for l in open(M+'en_dict.txt', encoding='utf-8')]),
        'ch': (ort.InferenceSession(M+'rec_ch.onnx', providers=['CPUExecutionProvider']), [l.rstrip('\n') for l in open(M+'ppocr_keys_v1.txt', encoding='utf-8')])}
MEAN=np.array([0.485,0.456,0.406],np.float32); STD=np.array([0.229,0.224,0.225],np.float32)

def detect(img):
    w,h = img.size; scale = min(1.0, 1280/max(w,h))
    nw, nh = max(32,int(round(w*scale/32))*32), max(32,int(round(h*scale/32))*32)
    x = np.asarray(img.resize((nw,nh), Image.BILINEAR), np.float32)/255.0
    x = ((x-MEAN)/STD).transpose(2,0,1)[None]
    prob = det.run(None, {'x': x})[0][0,0]           # nh x nw
    bit = prob > 0.3
    labels, n = ndimage.label(bit)
    boxes=[]
    for i, sl in enumerate(ndimage.find_objects(labels), 1):
        if sl is None: continue
        ys, xs = sl; y0,y1,x0,x1 = ys.start, ys.stop, xs.start, xs.stop
        mask = labels[sl]==i
        if mask.sum() < 10: continue
        score = prob[sl][mask].mean()
        if score < 0.5: continue
        # unclip: expand by area*ratio/perimeter (DB's polygon offset, on the box)
        bw, bh = x1-x0, y1-y0
        d = bw*bh*1.6/(2*(bw+bh))
        x0, y0, x1, y1 = x0-d, y0-d, x1+d, y1+d
        boxes.append([max(0,x0)/nw*w, max(0,y0)/nh*h, min(nw,x1)/nw*w, min(nh,y1)/nh*h, score])
    return boxes

def recognize(img, box, which):
    sess, dct = recs[which]
    x0,y0,x1,y1,_ = box
    crop = img.crop((int(x0),int(y0),int(np.ceil(x1)),int(np.ceil(y1))))
    ch=48; cw = max(16, int(round(crop.width*ch/crop.height)))
    x = np.asarray(crop.resize((cw,ch), Image.BILINEAR), np.float32)/255.0
    x = ((x-0.5)/0.5).transpose(2,0,1)[None]
    out = sess.run(None, {'x': x})[0][0]              # T x classes
    idx = out.argmax(1); conf = out.max(1)
    chars=[]; prev=0
    for t,(i,c) in enumerate(zip(idx,conf)):
        if i!=0 and i!=prev:
            chars.append(dct[i-1] if i-1 < len(dct) else ' ')
        prev=i
    return ''.join(chars), float(conf[idx!=0].mean()) if (idx!=0).any() else 0.0

def layout(lines):
    # rows by centre y, then x; big gaps -> ' | '
    lines = sorted(lines, key=lambda l: (l[0][1]+l[0][3])/2)
    rows=[]
    for b,t in lines:
        cy=(b[1]+b[3])/2; hgt=b[3]-b[1]
        if rows and abs(cy-rows[-1][0]) < 0.5*hgt: rows[-1][1].append((b,t)); rows[-1][0]=(rows[-1][0]+cy)/2
        else: rows.append([cy,[(b,t)]])
    out=[]
    for _,items in rows:
        items.sort(key=lambda it: it[0][0]); s=''; last=None
        for b,t in items:
            if last is not None: s += ' | ' if b[0]-last > 1.5*(b[3]-b[1]) else ' '
            s += t; last=b[2]
        out.append(s)
    return '\n'.join(out)

def words(s): return [w.strip('.,:;!?()[]"\'') for w in s.split()]
def acc(truth, got):
    t, g = words(truth), words(got); import collections
    tc = collections.Counter(t); hit = sum(min(c, collections.Counter(g)[w]) for w,c in tc.items())
    return hit/len(t)
cases=json.load(open('truth.json'))
for which in ['en','ch']:
    tot=0; t0=time.time()
    for c in cases:
        img=Image.open(c['file']).convert('RGB')
        boxes=detect(img)
        lines=[(b, recognize(img,b,which)[0]) for b in boxes]
        text=layout(lines); a=acc(c['text'], text); tot+=a
        if '-v' in sys.argv: print('---', c['file'], f'{a:.2f}', len(boxes), 'boxes\n'+text)
        else: print(f"{which} {c['file']:14s} boxes {len(boxes):2d} word-acc {a:.2f}")
    print(f"== {which}: mean word accuracy {tot/len(cases):.3f}, {(time.time()-t0)/len(cases)*1000:.0f} ms/image on CPU\n")
