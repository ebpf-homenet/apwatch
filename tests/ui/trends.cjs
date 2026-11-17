// Pure geometry regression. Run with Node; no browser/server/network required.
const fs = require('node:fs'), path = require('node:path'), vm = require('node:vm');
const assert = require('node:assert/strict');
const root = path.resolve(__dirname, '../..');
const ap = fs.existsSync(path.join(root, 'assets/index.html'));
const source = fs.readFileSync(path.join(root, ap ? 'assets/index.html' : 'web/assets/app.js'), 'utf8');
const geometry = source.slice(source.indexOf('// BEGIN TREND GEOMETRY'), source.indexOf('// END TREND GEOMETRY'));
const stack = ap ? source.slice(source.indexOf('function stackedTrendSlopes('), source.indexOf('/**\n * 保形曲线')) : '';
const ctx = vm.createContext({});
vm.runInContext(geometry + stack, ctx);
const {trendPath, trendSegments, stackedTrendSlopes} = ctx;
const commands = d => [...d.matchAll(/([MLCHVZ])([^MLCHVZ]*)/g)].map(m => ({op:m[1], v:m[2].trim().split(/[ ,]+/).filter(Boolean).map(Number)}));
const cubic = (a, b, c, d, t) => (1-t)**3*a + 3*(1-t)**2*t*b + 3*(1-t)*t*t*c + t**3*d;
function check(points, slopes) {
  const cmds = commands(trendPath(points, false, slopes));
  assert.equal(cmds.length, points.length);
  if (!points.length) return;
  assert.deepEqual(cmds[0], {op:'M',v:points[0]});
  for (let i=1;i<points.length;i++) {
    const [x0,y0]=points[i-1], [x,y]=points[i], c=cmds[i];
    assert.equal(c.op,points.length===2?'L':'C');
    assert.deepEqual(c.v.slice(-2),[x,y]);
    if(c.op==='L')continue;
    const [cx1,cy1,cx2,cy2]=c.v;
    let prev=y0;
    for(let j=0;j<=100;j++) {
      const u=j/100, value=cubic(y0,cy1,cy2,y,u);
      assert(value>=Math.min(y,y0)-1e-9 && value<=Math.max(y,y0)+1e-9, 'curve overshoots samples');
      assert((value-prev)*Math.sign(y-y0)>=-1e-9,'extra extremum');prev=value;
      assert(Math.abs(cubic(x0,cx1,cx2,x,u)-(x0+(x-x0)*u))<1e-8);
    }
    if(i>1) {
      const p=cmds[i-1].v;
      assert(Math.abs((y0-p[3])/(x0-p[2])-(cy1-y0)/(cx1-x0))<1e-7,'tangent discontinuity');
    }
  }
}
let seed=781;
const random=()=>((seed=(Math.imul(seed,1664525)+1013904223)>>>0)/2**32);
for(const y of [[],[5],[2,9],[4,4,4,4],[0,100,0,0,100],[0,1,100,101],[-90,-30,-85,-86]]){
 const points=y.map((v,i)=>[i*i+i,v]);check(points);check(points.slice().reverse());
}
for(let k=0;k<150;k++) {
 let x=0;const p=Array.from({length:15},()=>[x+=.01+random()*50,random()*200-100]);check(p);check(p.slice().reverse());
}
assert.equal(trendPath([[0,2],[3,7],[9,1]],true),'M0,2H3V7H9V1');
const plain = v => JSON.parse(JSON.stringify(v));
assert.deepEqual(plain(trendSegments([0,1,2,3,20,21,22],[1,null,3,4,5,NaN,6])),[[0],[2,3],[4],[6]]);
assert.deepEqual(plain(trendSegments([0,1,1,2,3],[1,2,3,4,5])),[[0,1],[2,3,4]]);
assert.deepEqual(plain(trendSegments([0,1,2],[0,0,0])),[[0,1,2]]);
assert.deepEqual(plain(trendSegments([0,1,2],[null,undefined,Infinity])),[]);
if(ap) for(let k=0;k<100;k++) {
 let x=0;const times=Array.from({length:12},()=>x+=1+random()*20);
 const rows=[times.map(x=>[x,0])];
 for(let j=0;j<3;j++)rows.push(rows[j].map(([x,y])=>[x,y+(random()<.3?0:random()*100)]));
 const slopes=stackedTrendSlopes(rows);
 rows.forEach((p,j)=>check(p,slopes[j]));
 const paths=rows.map((p,j)=>commands(trendPath(p,false,slopes[j])));
 for(let j=1;j<rows.length;j++)for(let i=1;i<times.length;i++)for(let q=0;q<=100;q++) {
  const a=paths[j-1][i].v,b=paths[j][i].v,u=q/100;
  const low=cubic(rows[j-1][i-1][1],a[1],a[3],a[5],u),high=cubic(rows[j][i-1][1],b[1],b[3],b[5],u);
  assert(high>=low-1e-9,'stacked bands cross');
 }
}
console.log(`${ap?'apwatch':'phdns'}: bounds, extrema, continuity, missing samples, steps${ap?', stacked non-crossing':''} passed`);
