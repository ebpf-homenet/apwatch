// Run on an isolated test host with Playwright. All network requests are intercepted.
const fs=require('node:fs'),path=require('node:path'),assert=require('node:assert/strict');
const {chromium}=require('playwright');
const root=path.resolve(__dirname,'../..'), app=JSON.parse(fs.readFileSync(path.join(__dirname,'project.json'))).name;
const state=require('./fixture.cjs')();
const out=process.env.UI_OUTPUT||'/tmp/'+app+'-ui-results';fs.mkdirSync(out,{recursive:true});
(async()=>{
 const browser=await chromium.launch({executablePath:process.env.CHROME_BIN,args:['--no-sandbox']});
 try {
 const page=await browser.newPage({viewport:{width:1440,height:1050},colorScheme:'light',reducedMotion:'reduce'});
 const errors=[],requests=[];let failure='',delay='';
 page.on('pageerror',e=>errors.push(e.message));
 await page.addInitScript(()=>{window.rejections=[];addEventListener('unhandledrejection',e=>window.rejections.push(String(e.reason)))});
 await page.route('**/*',async route=>{
  const req=route.request(),u=new URL(req.url()),p=u.pathname;
  if(p==='/')return route.fulfill({contentType:'text/html',body:fs.readFileSync(path.join(root,'assets/index.html'),'utf8')});
  if(!p.startsWith('/api/'))throw Error('Unexpected network '+u);
  const method=req.method(),body=req.headers()['content-type']?.includes('json')?req.postDataJSON():req.postData();
  requests.push({p,method,body,headers:req.headers()});
  if(p===delay)await new Promise(r=>setTimeout(r,350));
  if(p===failure)return route.fulfill({status:503,json:{error:'测试连接失败'}});
  const res=state.respond(u,body,method);
  if(res.binary)return route.fulfill({body:'fixture',contentType:'application/gzip',headers:{'content-disposition':'attachment; filename="fixture.tar.gz"'}});
  await route.fulfill(res.status?res:{json:res});
 });
 const wait=()=>page.waitForFunction(()=>!busy&&!refreshPending&&cur);
 const nav=async name=>{await page.evaluate(name=>{location.hash='#/'+name},name);await page.waitForFunction(name=>document.querySelector('#view').dataset.page===name.split('/')[0],name);await wait();await page.evaluate(()=>scrollTo(0,0));assert.equal(await page.locator('#page-error').textContent(),'','unexpected error on '+name);};
 const text=async(s,t)=>page.waitForFunction(([s,t])=>document.querySelector(s)?.textContent.includes(t),[s,t]);
 const fit=async()=>{const v=await page.evaluate(()=>({width:innerWidth,doc:document.documentElement.scrollWidth,overflow:[...document.querySelectorAll('main *')].filter(e=>e.getBoundingClientRect().right>innerWidth+2 && getComputedStyle(e).position!=='fixed'&&!e.closest('.tbl-wrap,.tbl-data')).slice(0,5).map(e=>e.outerHTML.slice(0,100))}));assert(v.doc<=v.width,JSON.stringify(v));
  const layout=await page.evaluate(()=>({top:document.querySelector('#view').getBoundingClientRect().top, desktop:innerWidth>860, stripe:getComputedStyle(document.querySelector('#nav .on')).boxShadow, headings:document.querySelectorAll('.page-head,.eyebrow,.breadcrumb,.top').length}));
  assert.equal(layout.headings,0);assert.equal(layout.stripe,'none');
  assert(layout.top<=(layout.desktop?24:64),'business content pushed down: '+JSON.stringify(layout));
 };
 const writes=()=>requests.filter(r=>r.method==='POST');
 const action=async(sel,expected)=>{const n=writes().length;await page.locator(sel).click();for(let i=0;i<100&&writes().length===n;i++)await page.waitForTimeout(20);assert.equal(writes().at(-1)?.p,expected);await wait();};
 await page.goto('http://'+app+'.test');await wait();assert.equal(await page.locator('#page-error').textContent(),'');assert.equal(writes().length,0);
 assert.equal(await page.evaluate(()=>getComputedStyle(document.body).backgroundColor),'rgb(250, 244, 237)');await fit();await page.screenshot({path:out+'/overview-light.png',fullPage:true});
 await page.locator('#theme').click();await page.locator('#theme').click();assert.equal(await page.locator('html').getAttribute('data-theme'),'dark');assert.equal(await page.evaluate(()=>getComputedStyle(document.body).backgroundColor),'rgb(35, 33, 54)');await page.screenshot({path:out+'/overview-dark.png',fullPage:true});
 await page.reload();await wait();assert.equal(await page.locator('html').getAttribute('data-theme'),'dark');await page.locator('#theme').click();await page.locator('#theme').click();
 if(app==='myapps'){
  assert.equal(await page.locator('#ov-b tr').count(),3);await page.locator('#ov-b tr').first().focus();await page.keyboard.press('Enter');await text('#ad-title','无线观测');await wait();
  const before=writes().length;await page.locator('[data-act="stop"]').click();await page.locator('#ad-out .no').click();assert.equal(writes().length,before);
  await page.locator('[data-act="stop"]').click();await action('#ad-out .go','/api/apps/apwatch/stop');await text('#ad-acts','启动');assert(state.data.apps[0].enabled,'stop must preserve autostart');
  await action('[data-act="start"]','/api/apps/apwatch/start');await action('[data-act="restart"]','/api/apps/apwatch/restart');
  await action('#ad-acts [data-act="disable"]','/api/apps/apwatch/disable');assert.equal(state.data.apps[0].enabled,false);
  await action('#ad-acts [data-act="enable"]','/api/apps/apwatch/enable');
  await action('[data-act="check"]','/api/apps/apwatch/check');await text('#ad-vout','有新版本');await action('#ad-vout .go','/api/apps/apwatch/update');
  await page.locator('[data-act="rollback"]').click();await action('#ad-vout .go','/api/apps/apwatch/rollback');
  for(const [act,endpoint] of [['upload','upload'],['restore','restore']]){
   const chooser=page.waitForEvent('filechooser');await page.locator(`[data-act="${act}"]`).click();await(await chooser).setFiles({name:act==='restore'?'data.tar.gz':'app.bin',mimeType:'application/octet-stream',buffer:Buffer.from('fixture')});
   await action(act==='restore'?'#ad-dout .go':'#ad-vout .go','/api/apps/apwatch/'+endpoint);
  }
  const download=page.waitForEvent('download');await page.locator('[data-act="backup"]').click();assert.equal((await download).suggestedFilename(),'fixture.tar.gz');await wait();assert(!await page.locator('[data-act="restore"]').isDisabled());
  await nav('events');await page.locator('[data-a="apwatch"]').click();await wait();assert.equal(await page.locator('#ev-l .ev').count(),1);
  await nav('settings');await page.locator('#proxy').fill('http://127.0.0.1:7890');await action('#save-proxy','/api/settings');assert.equal(state.settings.proxy,'http://127.0.0.1:7890');await action('#reload','/api/reload');
  const all=page.waitForEvent('download');await page.locator('#all-backup').click();await all;
 }else{
  await require('./trend-browser.cjs').probe(page);
  await page.locator('#ov-charts .tb').first().click();assert.equal(await page.locator('#ov-charts .tbl-data tbody tr').count(),60);
  await nav('clients');await page.locator('#cl-q').fill('工作');assert.equal(await page.locator('#cl-b tr[data-mac]').count(),1);await page.locator('#cl-h th').first().focus();await page.keyboard.press('Enter');await page.locator('#cl-b tr').focus();await page.keyboard.press('Enter');await text('#cd-name','工作电脑');await wait();
  await page.locator('#cd-mcs-btn').click();await text('#cd-mcs','MCS 9');
  await nav('devices');await page.locator('#dv-b .ed').first().click();await page.locator('#dv-edit .nm').fill('工作站');await action('#dv-edit .sv','/api/alias');await text('#dv-b','工作站');
  await page.locator('#dv-add').click();await page.locator('#dv-edit .mc').fill('02:00:00:00:00:04');await page.locator('#dv-edit .nm').fill('新设备');await action('#dv-edit .sv','/api/alias');await text('#dv-b','新设备');
  await nav('manage');let n=writes().length;await page.locator('[data-b="1"].tg').click();await page.locator('#mg-radio .no').click();assert.equal(writes().length,n);
  await page.locator('[data-b="1"].tg').click();await action('#mg-radio .go','/api/ctl/radio');assert.deepEqual(writes().at(-1).body,{band:1,enable:false,restore_mins:30});await page.waitForTimeout(900);await wait();
  await action('[data-b="1"].tg','/api/ctl/radio');await page.waitForTimeout(900);await wait();
  await page.locator('#mg-dev').selectOption('02:00:00:00:00:03');await page.locator('#mg-bgo').click();await action('#mg-bcf .go','/api/ctl/block');assert.equal(writes().at(-1).body.block,true);await page.waitForTimeout(1400);await wait();
  await action('#mg-blocked .ub','/api/ctl/block');await page.waitForTimeout(900);await wait();await action('.cx','/api/ctl/cancel');
  await action('#mg-out','/api/logout');await text('#mg-body','管理功能需要');await page.locator('#mg-login').click();assert(await page.locator('.modal').isVisible());await page.keyboard.press('Shift+Tab');assert.equal(await page.evaluate(()=>document.activeElement.textContent),'登录');await page.keyboard.press('Escape');assert(!await page.locator('.modal').count());
  await page.locator('#mg-login').click();await page.locator('.modal input').fill('fixture-password');await action('.modal .pri','/api/login');await text('#mg-body','自动恢复计划');
  await nav('settings');await page.locator('[name="fast_secs"]').fill('40');await action('#set-form .pri','/api/settings');assert.equal(writes().at(-1).body.fast_secs,40);assert(!('password' in writes().at(-1).body));
  await page.locator('#co-pause').click();await action('#co-pcf .go','/api/collector/pause');await page.waitForTimeout(500);await text('#co-top','恢复采集');await action('#co-resume','/api/collector/pause');
  await nav('events');await page.locator('#ev-q').fill('does-not-exist');await text('#ev-list','没有符合');
 }
 const pages=app==='myapps'?['overview','apps','app/apwatch','events','settings']:['overview','clients','devices','client/02:00:00:00:00:01','radio','events','manage','settings'];
 for(const width of [1440,768,390,320]){
  await page.setViewportSize({width,height:1000});
  for(const p of pages){await nav(p);await fit();if(app==='apwatch')await require('./trend-browser.cjs').rendered(page);if(width===390)await page.screenshot({path:out+'/'+p.split('/')[0]+'-mobile.png',fullPage:true});}
  if(width<=860){await page.locator('#menu-toggle').click();assert(await page.locator('#sidebar').isVisible());await page.locator('#nav a[data-k="overview"]').click();await page.waitForFunction(()=>document.querySelector('#view').dataset.page==='overview'&&!busy&&!refreshPending);assert.equal(await page.locator('#menu-toggle').getAttribute('aria-expanded'),'false');await page.locator('#menu-toggle').click();await page.locator('#nav a[data-k="overview"]').click();assert.equal(await page.locator('#menu-toggle').getAttribute('aria-expanded'),'false');}
  if(width===390){await nav('overview');await page.screenshot({path:out+'/overview-mobile.png',fullPage:true});}
 }
 await page.setViewportSize({width:1440,height:1050});
 // A delayed old page must never overwrite the current page or raise errors.
 await nav('overview');delay=app==='myapps'?'/api/apps':'/api/overview';
 await page.evaluate(()=>{void refresh(true)});await page.evaluate(()=>{location.hash='#/events'});await page.waitForTimeout(600);await wait();assert.equal(await page.locator('#page-title').textContent(),'事件');assert.equal(await page.locator('#page-error').textContent(),'');delay='';
 await nav('settings');failure=app==='myapps'?'/api/apps':'/api/overview';await page.evaluate(()=>{location.hash='#/overview'});await text('#page-error','数据更新失败');failure='';await page.locator('#retry').click();await wait();assert.equal(await page.locator('#page-error').textContent(),'');
 state.empty=true;await nav(app==='myapps'?'overview':'clients');await text(app==='myapps'?'#ov-b':'#cl-b',app==='myapps'?'还没有':'没有符合');
 for(const r of writes())assert.equal(r.headers[app==='myapps'?'x-myapps':'x-apw'],'1');
 assert.deepEqual(errors,[]);assert.deepEqual(await page.evaluate(()=>window.rejections),[]);
 console.log(JSON.stringify({app,passed:true,pages:pages.length,widths:[1440,768,390,320],requests:requests.length,writes:writes().length,errors},null,2));
 }finally{await browser.close();}
})().catch(e=>{console.error(e);process.exit(1)});
