const assert=require('node:assert/strict');
const app=require('./project.json').name;
const {chromium}=require('playwright');
(async()=>{
 const browser=await chromium.launch({executablePath:process.env.CHROME_BIN,args:['--no-sandbox']});
 try {
  const page=await browser.newPage({viewport:{width:1360,height:900}}),errors=[];
  page.on('pageerror',e=>errors.push(e.message));
  await page.route('**/*',route=>{const u=new URL(route.request().url());assert.equal(u.origin,process.env.UI_URL);return route.continue();});
  const wait=()=>page.waitForFunction(()=>cur&&!busy&&!refreshPending);
  const text=(s,t)=>page.waitForFunction(([s,t])=>document.querySelector(s)?.textContent.includes(t),[s,t]);
  const nav=async name=>{await page.evaluate(name=>location.hash='#/'+name,name);await page.waitForFunction(name=>document.querySelector('#view').dataset.page===name.split('/')[0],name);await wait();assert.equal(await page.locator('#page-error').textContent(),'');};
  await page.goto(process.env.UI_URL);await wait();assert.equal(await page.locator('#page-error').textContent(),'');
  if(app==='myapps'){
   await nav('app/demo');await text('#ad-title','隔离测试程序');await page.locator('[data-act="start"]').click();await text('#ad-st','运行中');await text('#ad-log','fixture ready');
   await page.locator('[data-act="stop"]').click();await page.locator('#ad-out .go').click();await text('#ad-st','已停止');
   const data=await(await page.request.get(process.env.UI_URL+'/api/apps')).json();assert.equal(data.apps[0].enabled,false);assert.equal(data.apps[0].status.state,'stopped');
   await nav('settings');await page.locator('#proxy').fill('http://127.0.0.1:7890');await page.locator('#save-proxy').click();await text('#set-msg1','已保存');
   assert.equal((await(await page.request.get(process.env.UI_URL+'/api/settings')).json()).proxy,'http://127.0.0.1:7890');
   const download=page.waitForEvent('download');await page.locator('#all-backup').click();assert((await download).suggestedFilename().endsWith('.tar.gz'));
   for(const p of ['overview','apps','events'])await nav(p);
  }else{
   await text('#ov-banner','采集已暂停');
   for(const p of ['clients','devices','radio','events','manage','settings'])await nav(p);
   await nav('devices');await page.locator('#dv-add').click();await page.locator('#dv-edit .mc').fill('02:00:00:00:00:09');await page.locator('#dv-edit .nm').fill('隔离测试设备');await page.locator('#dv-edit .sv').click();
   await page.locator('.modal input').fill('fixture-password');await page.locator('.modal .pri').click();await text('#dv-b','隔离测试设备');
   const devices=await(await page.request.get(process.env.UI_URL+'/api/devices')).json();assert(devices.devices.some(d=>d.alias==='隔离测试设备'));
   await nav('settings');await page.locator('[name="fast_secs"]').fill('40');await page.locator('#set-form .pri').click();await text('#set-msg','已保存');
   const settings=await(await page.request.get(process.env.UI_URL+'/api/settings')).json();assert.equal(settings.fast_secs,40);assert(settings.paused_since);assert(!('password' in settings));
   await page.locator('#theme').click();await page.locator('#theme').click();
   assert.equal(await page.locator('html').getAttribute('data-theme'),'dark');
   const switchTo=async(path,hash)=>{
    const link=page.locator('#ui-switch'), box=await link.boundingBox();
    assert(box && box.x>=0 && box.x+box.width<=page.viewportSize().width,'switch is reachable at this width');
    await Promise.all([page.waitForURL(url=>url.pathname===path&&url.hash===hash),link.click()]);
    await page.waitForFunction(()=>cur&&!busy);
   };
   await switchTo('/classic/','#/settings');
   assert.equal(await page.locator('[name="fast_secs"]').inputValue(),'40');
   assert.equal(await page.locator('html').getAttribute('data-theme'),'dark');
   assert((await(await page.request.get(process.env.UI_URL+'/api/session')).json()).logged_in);
   await page.evaluate(()=>{location.hash='#/devices';});await text('#dv-b','隔离测试设备');
   await page.setViewportSize({width:320,height:800});
   await switchTo('/','#/devices');await text('#dv-b','隔离测试设备');
   await nav('client/02:00:00:00:00:09');
   await switchTo('/classic/','#/client/02:00:00:00:00:09');
   await text('#cd-name','隔离测试设备');
   await switchTo('/','#/client/02:00:00:00:00:09');
   assert((await(await page.request.get(process.env.UI_URL+'/api/session')).json()).logged_in);
   const collector=await(await page.request.get(process.env.UI_URL+'/api/collector')).json();assert.equal(collector.connects,0);assert.equal(Object.keys(collector.cmds).length,0);
  }
  assert.deepEqual(errors,[]);console.log(app+': real browser flows PASS');
 }finally{await browser.close()}
})().catch(e=>{console.error(e);process.exit(1)});
