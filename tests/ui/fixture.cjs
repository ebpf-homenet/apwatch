// Synthetic AP data only. This fixture never contacts an AP or reads credentials.
module.exports = () => {
 const now=Math.floor(Date.now()/1000), mac='02:00:00:00:00:01';
 const live={mac,name:'工作电脑',alias:'工作电脑',hostname:'workstation',name_src:'alias',ip:'192.0.2.20',online:true,band:1,rssi:-48,snr:47,tx_rate:1201,rx_rate:960,mode:'11AXA_HE160',assoc_secs:8100,idle:0,assoc24:3,nss:2,chan:36,random_mac:true,first_seen:now-86400,last_seen:now,spark:[-52,-51,-48,-50,-48],max_rate:2402000};
 const devices=[live,{...live,mac:'02:00:00:00:00:02',name:'客厅电视',alias:'客厅电视',ip:'192.0.2.21',band:0,rssi:-65,tx_rate:286,rx_rate:143,mode:'11AXG_HE40'},{...live,mac:'02:00:00:00:00:03',name:'阅读平板',alias:'',online:false,last_seen:now-800,rssi:-70}];
 const radios=[0,1].map((i)=>({iface:{chan:i?36:6,width:i?160:40,center:i?5250:2437,txpower:23},ssid:i?'Home · 5 GHz':'Home · 2.4 GHz',mode:i?'11AXA_HE160':'11AXG_HE40',busy:i?21:35,tx:10,rx:7,clients:i?8:12,tx_mbps:i?86.2:4.8,rx_mbps:1.4,nf:-95,temp:i?63:58}));
 const events=[{t:now-60,kind:'online',name:live.name,mac,band:1,msg:'终端连接到 5 GHz'},{t:now-180,kind:'roam',name:'客厅电视',mac:devices[1].mac,band:0,msg:'设备切换频段'},{t:now-480,kind:'collector',msg:'采集连接已恢复'}];
 const overview={connected:true,ap_host:'192.0.2.3',last_poll:now,ap:{uptime:260100,cpu:7.3,mem_total:512000,mem_avail:312000},radios,score:96,findings:[],online:[12,8],assoc24:36,events,radar:[]};
 const collector={connected:true,connected_since:now-8100,last_poll:now,started:now-8100,connects:1,ap_cpu:7.3,paused_since:null,cmds:{stat:{cmd:'cat /proc/stat',every:'30 秒',count:270,last_ms:22,avg_ms:23,last_bytes:520,errors:0,last_t:now}},store:{series:12,samples:26004,events:300},whitelist:['cat /proc/stat','wlanconfig ath0 list sta']};
 const settings={ap_host:'192.0.2.3',password_set:true,unlock_set:true,fast_secs:30,dmesg_secs:60,slow_secs:300,save_mins:360,leases_path:'/tmp/dhcp.leases',bounds:{fast_secs:[10,600],dmesg_secs:[30,3600],slow_secs:[60,3600],save_mins:[5,1440]}};
 const control={radio:[true,true],blocked:[],plans:[{id:1,desc:'恢复 5 GHz',at:now+1800}]};
 const vals={busy:28,busy_max:38,tx:10,rx:7,other:11,self:12,obss:16,tx_mbps:48,rx_mbps:12,clients:8,width:160,nf:-95,phy_err_ps:2,temp:60,retry_pct:3,auth_att:2,auth_ok:2,tx_fail:0,cpu:7,mem_pct:39,rssi:-48,rssi_min:-58};
 const series={t:Array.from({length:60},(_,i)=>now-3540+i*60),v:Object.fromEntries(Object.entries(vals).map(([k,v])=>[k,Array.from({length:60},(_,i)=>v+(i%7-3)*(k==='width'?0:Math.abs(v)*.05))]))};
 const state={logged:true,empty:false,overview,collector,settings,devices,control};
 state.respond=(u,body,method)=>{
  const p=u.pathname;
  if(method==='POST'){
   if(p==='/api/login'){state.logged=true;return {ok:true};}
   if(p==='/api/logout'){state.logged=false;return {ok:true};}
   if(!state.logged)return {status:401,json:{error:'需要登录'}};
   if(p==='/api/settings'){Object.assign(settings,body);return {ok:true};}
   if(p==='/api/alias'){const d=devices.find(d=>d.mac===body.mac);if(d){d.name=body.name||d.hostname;d.alias=body.name;}else devices.push({...live,mac:body.mac,name:body.name,alias:body.name,online:false});return {ok:true};}
   if(p==='/api/collector/pause'){collector.paused_since=body.paused?now:null;overview.paused_since=collector.paused_since;return {ok:true};}
   if(p==='/api/ctl/radio'){control.radio[body.band]=body.enable;return control;}
   if(p==='/api/ctl/block'){control.blocked=control.blocked.filter(d=>d.mac!==body.mac);if(body.block)control.blocked.push(devices.find(d=>d.mac===body.mac)||{mac:body.mac});return control;}
   if(p==='/api/ctl/cancel'){control.plans=control.plans.filter(x=>x.id!==body.id);return control;}
  }
  if(p==='/api/session')return {logged_in:state.logged};
  if(p==='/api/overview')return overview;
  if(p==='/api/collector')return collector;
  if(p==='/api/settings')return settings;
  if(p==='/api/clients')return {clients:state.empty?[]:devices};
  if(p==='/api/devices')return {devices:state.empty?[]:devices};
  if(p==='/api/ctl')return state.logged?control:{status:401,json:{error:'需要登录'}};
  if(p==='/api/events')return {events:state.empty?[]:events,total:events.length,counts:{online:1,roam:1,collector:1}};
  if(p==='/api/series')return {from:now-3600,to:now,data:state.empty?{t:[],v:Object.fromEntries(Object.keys(vals).map(k=>[k,[]]))}:series};
  if(p==='/api/client')return {...live,live,series,events};
  if(p==='/api/sta')return {tx_mcs:[['MCS 7',30],['MCS 9',60]],rx_mcs:[['MCS 7',40]],ru:[['RU26',0]],stats:[['发送包数',90]]};
  throw Error('Unhandled fixture: '+method+' '+p);
 };
 return state;
};
