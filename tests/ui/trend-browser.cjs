const assert = require('node:assert/strict');
exports.probe = async page => {
 const result = await page.evaluate(() => {
  const el = document.createElement('div');el.id='curve-probe';el.style.width='600px';document.querySelector('#view').append(el);
  const opts={t:[0,1,2,3,4,5,6,20,21,22],series:[{name:'原始样本',color:'--s1',values:[12.34,18,15,null,21,27,24,27,23,29]}],area:true,tipFmt:v=>v.toFixed(2)};
  const c=chart(el,opts);window.curveProbe=c;
  const paths=()=>[...el.querySelectorAll('path')].map(p=>p.getAttribute('d'));
  const smooth=paths();c.opts={...opts,step:true};c.draw();const step=paths();
  c.opts={...opts,stacked:true,series:[...opts.series,{name:'第二层',color:'--s2',values:[1,1,1,1,1,1,1,1,1,1]}]};c.draw();const stack=paths();
  c.opts=opts;c.draw();return {smooth,step,stack};
 });
 for(const paths of [result.smooth,result.step,result.stack])for(const d of paths){assert(!/NaN|Infinity/.test(d));assert.equal((d.match(/M/g)||[]).length,3,'gap must split both line and fill');}
 const [area,line]=result.smooth;assert(line.includes('C'));assert(area.startsWith(line.split('M').slice(0,2).join('M')));
 assert(result.step.every(p=>p.includes('H')&&p.includes('V')&&!p.includes('C')),'step fill must match step line');
 assert(result.stack.every(p=>p.includes('C')));
 await page.locator('#curve-probe .hit').hover({position:{x:1,y:20}});
 assert((await page.locator('#tip').innerText()).includes('12.34'),'tooltip must show original sample');
 await page.evaluate(()=>{charts.delete(window.curveProbe);delete window.curveProbe;document.querySelector('#curve-probe').remove();hideTip();});
};
exports.rendered = async page => {
 const charts = await page.evaluate(()=>[...window.document.querySelectorAll('.chart')].flatMap(el=> {
  if(!el.querySelector('svg'))return [];
  const title=el.closest('.card')?.querySelector('h3')?.textContent||'';
  return [{title,paths:[...el.querySelectorAll('path')].map(p=>p.getAttribute('d'))}];
 }));
 for(const c of charts) {
  for(const d of c.paths)assert(!/NaN|Infinity/.test(d),c.title);
  if(['在线终端数','工作频宽','认证与发送失败','协商速率'].includes(c.title)) {
   assert(c.paths.length);for(const d of c.paths)assert(d.includes('H')&&!d.includes('C'),c.title);
  } else if(c.paths.length) assert(c.paths.some(d=>d.includes('C')),'continuous chart remains straight: '+c.title);
 }
};
