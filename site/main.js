const $=s=>document.querySelector(s),$$=s=>[...document.querySelectorAll(s)];
const RM=matchMedia('(prefers-reduced-motion:reduce)').matches;
/* 语言 */
const q=new URLSearchParams(location.search).get('lang');
let lang=q||localStorage.lang||(navigator.language.startsWith('zh')?'zh':'en');
const els=$$('[data-en]');els.forEach(e=>e._zh=e.innerHTML);
const T={zh:'一个 SSH 连接,管好所有服务器 — Otter',en:'Otter — One SSH, every server under control'};
function setLang(l){lang=l;localStorage.lang=l;document.documentElement.lang=l;document.title=T[l];
  els.forEach(e=>e.innerHTML=l==='en'?e.dataset.en:e._zh);$('#lang').textContent=l==='en'?'中文':'EN';hint();term()}
$('#lang').onclick=()=>setLang(lang==='zh'?'en':'zh');
/* 滚动淡入 + 数字 */
const io=new IntersectionObserver(es=>es.forEach(e=>{if(e.isIntersecting){e.target.classList.add('in');io.unobserve(e.target);
  e.target.querySelectorAll('[data-n]').forEach(count)}}),{threshold:.15});
$$('.rv').forEach(e=>io.observe(e));
function count(e){const n=+e.dataset.n,d=+e.dataset.d||0,s=e.dataset.s||'';if(RM||!n){e.textContent=n.toFixed(d)+s;return}
  const t0=performance.now();(function f(t){const p=Math.min(1,(t-t0)/1400),v=n*(1-Math.pow(1-p,3));e.textContent=v.toFixed(d)+s;if(p<1)requestAnimationFrame(f)})(t0)}
/* 苹果式分屏:按距离视口中心的远近驱动内容缩放/淡入 */
const secs=$$('section'),dots=$('#dots');
secs.forEach((s,i)=>{const a=document.createElement('a');a.href='#';a.onclick=e=>{e.preventDefault();s.scrollIntoView({behavior:RM?'auto':'smooth'})};dots.append(a)});
function panels(){let best=0,bd=9;secs.forEach((s,i)=>{const r=s.getBoundingClientRect(),d=Math.abs((r.top+r.height/2)-innerHeight/2)/innerHeight;
  const p=Math.max(0,1-Math.max(0,d-.12)*1.8);const mo=matchMedia('(max-width:860px)').matches;if(mo&&(r.bottom<-300||r.top>innerHeight+300))return;[...s.children].forEach(c=>{let q=p;if(mo){const cr=c.getBoundingClientRect();q=Math.max(0,Math.min(1,(innerHeight*.97-cr.top)/(innerHeight*.28)))}c.style.setProperty('--p',q.toFixed(3))});if(d<bd){bd=d;best=i}});
  [...dots.children].forEach((a,i)=>a.classList.toggle('on',i===best))}
let settled=false,st=0;const flats=$$('.flatimg'),scs=$$('.sc3');let cur=0;
let _pr=0;const _pf=()=>{_pr=0;panels()};addEventListener('scroll',()=>{if(!_pr)_pr=requestAnimationFrame(_pf)},{passive:true});addEventListener('resize',panels);panels();
/* 光斑 + 卡片高亮 + hero 倾斜 */
const spot=$('#spot');
addEventListener('pointermove',e=>{spot.style.transform=`translate(${e.clientX-innerWidth/2}px,${e.clientY-innerHeight*.3}px)`;
  const c=e.target.closest&&e.target.closest('.card');if(c){const r=c.getBoundingClientRect();c.style.setProperty('--mx',e.clientX-r.left+'px');c.style.setProperty('--my',e.clientY-r.top+'px')}});
/* 全局舞台:一个固定的 3D 舞台,在各屏的占位之间滑动、翻转,并切换场景 */
const gs=$('#gs'),tilt3=$('#tilt3');let mx=0,my=0;
const stSecs=[$('#top'),...$$('.feat')],slots=[$('#st3'),...$$('.feat .slot')];
const lerp=(a,b,t)=>a+(b-a)*t,ease=t=>t*t*(3-2*t),cl=(v,a=0,b=1)=>Math.max(a,Math.min(b,v));
function snapRect(i){const s=stSecs[i],e=slots[i];return{x:s.offsetLeft+e.offsetLeft,y:e.offsetTop,w:e.offsetWidth,h:e.offsetHeight}}
function setScene(i){if(i===cur)return;cur=i;scs.forEach((s,k)=>s.classList.toggle('on',k===i))}
const MQ=matchMedia('(max-width:860px)');
/* 手机:不吸附滚动,舞台直接贴着当前最显眼的占位走,停稳后切平面(录屏) */
function tlm(){const h=innerHeight;let best=-1,ba=0;
  slots.forEach((e,i)=>{if(!e.offsetWidth)return;const r=e.getBoundingClientRect(),a=Math.max(0,Math.min(r.bottom,h)-Math.max(r.top,0))/r.height;if(a>ba){ba=a;best=i}});
  if(best<0||ba<.15){gs.style.opacity=0;gs.classList.remove('sink','fl');settled=false;clearTimeout(st);st=0;return}
  const r=slots[best].getBoundingClientRect();
  gs.style.width=r.width+'px';gs.style.height=r.height+'px';gs.style.transform=`translate(${r.left}px,${r.top}px)`;gs.style.opacity=1;
  const hero=best===0?cl(1-scrollY/(h*.45)):0,cy=(r.top+r.height/2-h/2)/h;
  tilt3.style.setProperty('--rx',(best===0?40*ease(hero):cl(cy,-1,1)*-22)+'deg');tilt3.style.setProperty('--ry',(best>=1?cl(cy,-1,1)*10:0)+'deg');
  tilt3.style.setProperty('--s',lerp(1,.86,ease(hero)));tilt3.style.setProperty('--k',lerp(.45,1,ease(hero)));
  const k=cl(best-1,0,scs.length-1);setScene(k);
  const want=(best>=1&&ba>.8)||(best===0&&hero<.03);
  if(want){if(!settled&&!st)st=setTimeout(()=>{settled=true;st=0;tlm()},500)}else{settled=false;clearTimeout(st);st=0}
  gs.classList.toggle('sink',want);gs.classList.toggle('fl',settled&&want);
  flats.forEach((fl,j)=>{const o=j===k;fl.classList.toggle('on',o);if(fl.tagName==='VIDEO'){if(o&&fl.paused)fl.play().catch(()=>{});else if(!o&&!fl.paused)fl.pause()}})}
function tl(){if(MQ.matches)return tlm();const h=innerHeight,sy=scrollY,tops=stSecs.map(s=>s.offsetTop),n=stSecs.length-1;
  let i=0;for(let k=0;k<=n;k++)if(tops[k]<=sy+2)i=k;
  const t=i<n?cl((sy-tops[i])/(tops[i+1]-tops[i])):0,e=ease(t),A=snapRect(i),B=snapRect(Math.min(n,i+1));
  const x=lerp(A.x,B.x,e),y=lerp(A.y,B.y,e),w=lerp(A.w,B.w,e),ht=lerp(A.h,B.h,e);
  const after=i>=n?sy-tops[n]:0;
  gs.style.width=w+'px';gs.style.height=ht+'px';gs.style.transform=`translate(${x}px,${y-after}px)`;
  gs.style.opacity=cl(1-after/(h*.6));
  const f=i+t,hero=cl(1-f);                                   /* 首屏斜放程度:1=斜放,0=正面 */
  const swing=Math.sin(Math.PI*t)*(i>=1&&i<n?26:0)*((B.x>A.x)?-1:1);
  tilt3.style.setProperty('--rx',62*ease(hero)+my*-4+'deg');
  tilt3.style.setProperty('--ry',swing+mx*6+'deg');
  tilt3.style.setProperty('--s',lerp(1,.76,ease(hero)));
  tilt3.style.setProperty('--k',lerp(.32,1,ease(hero)));
  const idx=Math.round(f),rest=t<.01||t>.99;
  setScene(cl(idx-1,0,scs.length-1));
  const want=rest&&idx>=1&&idx<=n;
  if(want){if(!settled&&!st)st=setTimeout(()=>{settled=true;st=0;tl()},900)}else{settled=false;clearTimeout(st);st=0}
  gs.classList.toggle('sink',want);gs.classList.toggle('fl',settled&&want);flats.forEach((fl,k)=>{const o=k===cl(idx-1,0,scs.length-1);fl.classList.toggle('on',o);if(fl.tagName==='VIDEO'){if(o&&fl.paused)fl.play().catch(()=>{});else if(!o&&!fl.paused)fl.pause()}})}
addEventListener('pointermove',e=>{mx=e.clientX/innerWidth-.5;my=e.clientY/innerHeight-.5;tl()});
let _tr=0;const _tf=()=>{_tr=0;tl()};addEventListener('scroll',()=>{if(!_tr)_tr=requestAnimationFrame(_tf)},{passive:true});addEventListener('resize',tl);
tl();
/* 首屏元素依次落下 */
scs[0].classList.remove('on');setTimeout(()=>scs[cur].classList.add('on'),250);
/* 终端打字 */
const LINES=[['$ ssh root@prod-1','pr'],['Connected. Nothing installed.','ok'],['$ docker ps --format "{{.Names}}"','pr'],['nginx  postgres  redis  app-api','dm'],['$ du -sh /var/lib/docker','pr'],['38G   /var/lib/docker','dm'],['# Otter shows all of this as a GUI —','dm'],['# with zero bytes added to the server.','ok']];
let tk=0;
async function term(){const my=++tk,pre=$('#term');pre.innerHTML='';
  for(const [t,c] of LINES){const sp=document.createElement('span');sp.className=c;pre.append(sp,'\n');
    for(const ch of t){if(my!==tk)return;sp.textContent+=ch;if(!RM)await new Promise(r=>setTimeout(r,c==='pr'?38:8))}
    if(!RM)await new Promise(r=>setTimeout(r,c==='pr'?260:420))}
  if(my===tk)pre.insertAdjacentHTML('beforeend','<span class="cur"></span>')}
let started=false;new IntersectionObserver((es,o)=>{if(es[0].isIntersecting&&!started){started=true;term();o.disconnect()}},{threshold:.4}).observe($('#term'));
/* 跑马灯 */
fetch('https://raw.githubusercontent.com/chenbstack/otter/main/apps/catalog.json').then(r=>r.json()).then(d=>{const a=(d.apps||d).map(x=>x.name);fill(a)}).catch(()=>fill(['MySQL','PostgreSQL','Redis','MinIO','Nginx','Portainer','Gitea','Vaultwarden']));
function fill(a){const h=a.concat(a).map(n=>`<span>${n}</span>`).join('');$('#mq').innerHTML=h+h}
/* 系统识别 + 下载 */
const win=/Win/i.test(navigator.platform+navigator.userAgent),mac=/Mac/i.test(navigator.platform);
function hint(){const o=win?'Windows':mac?'macOS':null;$('#hint').textContent=o?(lang==='en'?`Detected ${o}`:`已识别你的系统:${o}`):'macOS · Windows'}
setLang(lang);
$$('.hs').forEach(h=>{const t=h.querySelector('.hs-track');const step=()=>t.querySelector('figure').offsetWidth+28;
h.querySelector('.hs-p').onclick=()=>t.scrollBy({left:-step(),behavior:'smooth'});h.querySelector('.hs-n').onclick=()=>t.scrollBy({left:step(),behavior:'smooth'});
let dn=0,sx=0,sl=0;t.addEventListener('pointerdown',e=>{if(e.pointerType!=='mouse')return;dn=1;sx=e.clientX;sl=t.scrollLeft;t.classList.add('drag')});
addEventListener('pointermove',e=>{if(dn)t.scrollLeft=sl-(e.clientX-sx)});addEventListener('pointerup',()=>{dn=0;t.classList.remove('drag')})});

$$('.hs').forEach(h=>{const t=h.querySelector('.hs-track'),fs=[...t.querySelectorAll('figure')];
const upd=()=>{const c=t.scrollLeft+t.clientWidth/2;fs.forEach(f=>{const d=Math.min(1,Math.abs(f.offsetLeft+f.offsetWidth/2-c)/(f.offsetWidth*1.1));f.style.setProperty('--fs',(1-.14*d).toFixed(3));f.style.setProperty('--fo',(1-.55*d).toFixed(3))})};
t.addEventListener('scroll',upd,{passive:true});upd();
new IntersectionObserver(([e])=>{if(e.isIntersecting&&!h.classList.contains('in')){h.classList.add('in');setTimeout(()=>h.classList.add('live'),1100)}},{threshold:.3}).observe(h)});

$$('.hs').forEach(h=>{const t=h.querySelector('.hs-track'),fs=[...t.querySelectorAll('figure')],bars=h.querySelector('.hs-bars');
bars.innerHTML=fs.map(()=>'<i></i>').join('');const bs=[...bars.children];let cur=-1,tm,lock=0,vis=0;
const go=(i,sc)=>{cur=i;fs.forEach((f,k)=>{f.classList.remove('on');bs[k].classList.remove('on');bs[k].classList.toggle('done',k<i)});void h.offsetWidth;fs[i].classList.add('on');{const cap=h.querySelector('.hs-cap'),fc=fs[i].querySelector('figcaption'),en=document.documentElement.lang==='en';if(cap&&fc){const b=fc.querySelector('b'),sp=fc.querySelector('span');cap.style.animation='none';void cap.offsetWidth;cap.style.animation='';cap.querySelector('b').textContent=en?b.dataset.en:b.textContent;cap.querySelector('span').textContent=en?sp.dataset.en:sp.textContent}}bs[i].classList.add('on');
if(sc){lock=Date.now()+900;t.scrollTo({left:fs[i].offsetLeft-(t.clientWidth-fs[i].offsetWidth)/2,behavior:'smooth'})}
clearTimeout(tm);tick(5500)};
const tick=ms=>{tm=setTimeout(()=>{if(vis&&!h.classList.contains('paused'))go((cur+1)%fs.length,1);else tick(500)},ms)};
bs.forEach((b,k)=>b.onclick=()=>go(k,1));h.querySelector('.hs-p').onclick=null;h.querySelector('.hs-n').onclick=null;
h.querySelector('.hs-p').addEventListener('click',()=>go((cur+fs.length-1)%fs.length,1));h.querySelector('.hs-n').addEventListener('click',()=>go((cur+1)%fs.length,1));
h.addEventListener('pointerenter',()=>h.classList.add('paused'));h.addEventListener('pointerleave',()=>h.classList.remove('paused'));
t.addEventListener('scroll',()=>{if(Date.now()<lock)return;const c=t.scrollLeft+t.clientWidth/2;let b=0,bd=1e9;fs.forEach((f,k)=>{const d=Math.abs(f.offsetLeft+f.offsetWidth/2-c);if(d<bd){bd=d;b=k}});if(b!==cur)go(b,0)},{passive:true});
new IntersectionObserver(([e])=>{const was=vis;vis=e.isIntersecting;if(vis&&!was)go(0,1)},{threshold:.5}).observe(h)});

/* HUD 滚动百分比 + 舞台光标高光 */
(()=>{const hp=$('#hp'),gs=$('#gs');const up=()=>{const p=Math.round(scrollY/Math.max(1,document.documentElement.scrollHeight-innerHeight)*100);hp.textContent=String(p).padStart(3,'0')};
addEventListener('scroll',up,{passive:true});up();
addEventListener('mousemove',e=>{const r=gs.getBoundingClientRect();gs.style.setProperty('--gx',e.clientX-r.left+'px');gs.style.setProperty('--gy',e.clientY-r.top+'px')},{passive:true})})();
/* 标题解码动画:首次进入视口时字符随机跳动后逐个定格 */
(()=>{if(RM)return;const P='01<>/_#$%&*+=?';
function texts(el){const w=document.createTreeWalker(el,NodeFilter.SHOW_TEXT),a=[];let n;while(n=w.nextNode())if(n.nodeValue.trim())a.push(n);return a}
function dec(el){const ns=texts(el).map(n=>({n,t:n.nodeValue}));const tot=ns.reduce((s,o)=>s+o.t.length,0);const t0=performance.now(),D=900;
(function f(t){const p=Math.min(1,(t-t0)/D),k=Math.floor(p*tot*1.0);let i=0;ns.forEach(o=>{let s='';for(const ch of o.t){s+=(i<k||/\s/.test(ch))?ch:P[Math.random()*P.length|0];i++}o.n.nodeValue=s});if(p<1)requestAnimationFrame(f);else ns.forEach(o=>o.n.nodeValue=o.t)})(t0)}
const io2=new IntersectionObserver(es=>es.forEach(e=>{if(e.isIntersecting){io2.unobserve(e.target);dec(e.target)}}),{threshold:.6});
$$('.feat h3,#stats h2').forEach(h=>io2.observe(h))})();

/* 背景数据流 + 实时曲线 + 舞台角标 */
(()=>{const rnd=n=>Array.from({length:n},()=>(Math.random()*0xffffffff>>>0).toString(16).padStart(8,'0')).join(' ');
const lg=['ssh-ed25519 handshake ok','docker ps -a  24 containers','pg_dump app > backup.sql','certbot renew --quiet','sftp put release.tar.gz','systemctl status nginx','crontab -l','du -sh /var/lib/docker'];
const L=$('#bgdata .l'),R=$('#bgdata .r');const col=(k)=>{let t='';for(let i=0;i<70;i++)t+=(k?rnd(3):lg[i%lg.length].padEnd(30,' ')+rnd(1))+'\n';return t+t};L.textContent=col(0);R.textContent=col(1);
$('#gs').insertAdjacentHTML('beforeend','<div class="crn"></div>');
if(RM)return;
const hc=$('#hc'),lc=$('#lc'),lt=$('#lcpu');const hx=hc.getContext('2d'),lx=lc.getContext('2d');let _s=.4;const A=Array.from({length:48},()=>(_s=Math.min(.9,Math.max(.15,_s+(Math.random()-.5)*.3))));let _t=.35;const B=Array.from({length:60},()=>(_t=Math.min(.85,Math.max(.15,_t+(Math.random()-.5)*.2))));
function draw(x,cv,d,col,fill){const w=cv.width,h=cv.height;x.clearRect(0,0,w,h);x.beginPath();d.forEach((v,i)=>{const X=i/(d.length-1)*w,Y=h-2-v*(h-4);i?x.lineTo(X,Y):x.moveTo(X,Y)});x.strokeStyle=col;x.lineWidth=2;x.stroke();if(fill){x.lineTo(w,h);x.lineTo(0,h);x.closePath();x.fillStyle=fill;x.fill()}}
let cpu=.32;setInterval(()=>{A.push(Math.min(.95,Math.max(.1,A[A.length-1]+(Math.random()-.5)*.3)));A.shift();draw(hx,hc,A,'#a78bfa');
cpu=Math.min(.9,Math.max(.12,cpu+(Math.random()-.5)*.14));B.push(cpu);B.shift();draw(lx,lc,B,'#4ade80','rgba(74,222,128,.15)');lt.textContent=Math.round(cpu*100)+'%'},400)})();

/* 概览平面图:CPU 曲线区域盖一层实时跳动的柱状图 */
(()=>{if(RM||1)return;const gs=$('#gs'),fl=$$('.flatimg');const cv=document.createElement('canvas');cv.width=1420;cv.height=194;
cv.style.cssText='position:absolute;left:16.8%;top:40.2%;width:50.7%;height:11%;display:none;pointer-events:none;z-index:2;border-radius:4px';gs.append(cv);const x=cv.getContext('2d');
const N=118,D=Array.from({length:N},(_,i)=>.35+.25*Math.sin(i/9));let ph=0;
const sync=()=>{const i=fl.findIndex(f=>f.classList.contains('on'));cv.style.display=(i===5&&gs.classList.contains('fl'))?'block':'none'};
new MutationObserver(sync).observe(gs,{attributes:true,subtree:true,attributeFilter:['class']});sync();
setInterval(()=>{if(cv.style.display==='none')return;ph+=.5;D.push(Math.min(.95,Math.max(.12,.4+.22*Math.sin(ph/4)+(Math.random()-.5)*.14)));D.shift();
x.clearRect(0,0,1420,194);x.fillStyle='#0b0914';x.fillRect(0,0,1420,194);const bw=1420/N;
D.forEach((v,i)=>{const h=v*186,g=x.createLinearGradient(0,194-h,0,194);g.addColorStop(0,v>.55?'#fb923c':'#d9f99d');g.addColorStop(1,'#4ade80');x.fillStyle=g;x.fillRect(i*bw+1,194-h,bw-3,h)})},140)})();
