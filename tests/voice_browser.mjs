// Start the isolated serve_synthetic_media fixture first (see docs/backend/crew-voice.md).
import assert from 'node:assert/strict';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const origin=process.env.GS_BROWSER_TEST_ORIGIN||'http://127.0.0.1:19090';
const browser=await chromium.launch({headless:true,executablePath:process.env.CHROME_BINARY,args:['--use-fake-device-for-media-stream','--use-fake-ui-for-media-stream','--autoplay-policy=no-user-gesture-required']});
const errors=[];
const clients=[];
try {
  for(const username of ['producer','administrator']) {
    const context=await browser.newContext({permissions:['microphone']});
    const page=await context.newPage();page.on('pageerror',e=>errors.push(e.message));
    // Only stub the login UI. WebSocket and archive authorization use real fixture sessions.
    await page.route('**/assets/media-login.js',route=>route.fulfill({contentType:'text/javascript',body:`export async function signIn(){return {token:'synthetic-${username}'}}`}));
    await page.addInitScript(()=>{
      window.audioSent=0;window.audioReceived=0;window.voiceSocket=null;
      const WS=window.WebSocket;
      window.WebSocket=class extends WS {
        constructor(...args){super(...args);window.voiceSocket=this;this.addEventListener('message',e=>{if(e.data instanceof ArrayBuffer)window.audioReceived++;});}
        send(data){if(data instanceof ArrayBuffer)window.audioSent++;return super.send(data);}
      };
    });
    await page.goto(`${origin}/radio`);await page.fill('#username',username);await page.fill('#password','fixture');await page.click('#signin');
    await page.click('#join');await page.waitForFunction(()=>document.querySelector('#status').textContent==='Connected to crew voice.');
    clients.push({context,page});
  }
  const [a,b]=clients.map(c=>c.page);
  const api=clients[0].context.request;
  await a.waitForFunction(()=>document.querySelector('#count').textContent==='2 / 12');
  assert.equal(await a.evaluate(()=>audioSent),0,'joining defaults to no transmitted audio');
  await a.bringToFront();
  await a.locator('#ptt').scrollIntoViewIfNeeded();
  const box=await a.locator('#ptt').boundingBox();await a.mouse.move(box.x+20,box.y+20);await a.mouse.down();
  await b.waitForFunction(()=>audioReceived>3);
  await a.mouse.up();await a.waitForTimeout(150);
  const before=await a.evaluate(()=>audioSent);await a.waitForTimeout(200);
  assert.equal(await a.evaluate(()=>audioSent),before,'release stops sending');
  await a.selectOption('#mode','open');await a.waitForFunction(n=>audioSent>n+3,before);
  await a.click('#mute');await a.waitForTimeout(150);const muted=await a.evaluate(()=>audioSent);await a.waitForTimeout(200);
  assert.equal(await a.evaluate(()=>audioSent),muted,'mute stops open mic');
  await a.click('#mute');await a.selectOption('#mode','ptt');
  await a.locator('#ptt').focus();await a.keyboard.down('Space');await a.waitForFunction(n=>audioSent>n+3,muted);await a.keyboard.up('Space');
  await a.locator('#ptt').focus();await a.keyboard.down('Space');await a.evaluate(()=>window.dispatchEvent(new Event('blur')));await a.keyboard.up('Space');
  assert.equal(await a.locator('#ptt').getAttribute('aria-pressed'),'false','blur releases PTT');
  await b.bringToFront();await b.selectOption('#mode','open');await a.waitForFunction(()=>audioReceived>3);await b.click('#mute');
  // Audience listens through the delayed program and cannot open a transmit socket.
  const managerHeaders={Authorization:'Bearer synthetic-producer'};
  const viewerHeaders={Authorization:'Bearer synthetic-viewer'};
  const config=await (await api.get(`${origin}/api/live_streams`,{headers:managerHeaders})).json();
  let broadcast={...config.broadcast,comms_audio_enabled:true,delay_seconds:3};
  const forbidden=await api.post(`${origin}/api/live_streams/control`,{headers:viewerHeaders,data:broadcast});assert.equal(forbidden.status(),403);
  const saved=await api.post(`${origin}/api/live_streams/control`,{headers:managerHeaders,data:broadcast});assert.equal(saved.status(),200);broadcast=await saved.json();
  const viewerConfig=await (await api.get(`${origin}/api/live_streams`,{headers:viewerHeaders})).json();
  const audioUrl=origin+viewerConfig.program_url.replace('/program?','/program/audio?');
  const early=await (await api.get(audioUrl)).json();assert.equal(early.frames.length,0,'no live audio before delay');
  const spectator=await browser.newContext();const audience=await spectator.newPage();audience.on('pageerror',e=>errors.push(e.message));
  await audience.addInitScript(()=>{
    window.programFrames=0;
    navigator.mediaDevices.getUserMedia=()=>{throw Error('audience must not request microphone access');};
    const original=AudioBufferSourceNode.prototype.start;
    AudioBufferSourceNode.prototype.start=function(...args){window.programFrames++;return original.apply(this,args);};
  });
  await audience.goto(origin+viewerConfig.program_url);
  await audience.waitForFunction(()=>document.querySelector('#stream-sound button'));
  if(await audience.getByRole('button',{name:'Unmute stream',exact:true}).count())await audience.getByRole('button',{name:'Unmute stream',exact:true}).click();
  await a.selectOption('#mode','open');
  await audience.waitForFunction(()=>programFrames>3,{},{timeout:15000});
  await a.click('#mute');
  const viewerDenied=await audience.evaluate(origin=>new Promise(resolve=>{
    const ws=new WebSocket(origin.replace('http','ws')+'/api/voice/ws');ws.onopen=()=>ws.send(JSON.stringify({type:'auth',token:'synthetic-viewer'}));
    ws.onmessage=e=>{resolve(JSON.parse(e.data));ws.close();};setTimeout(()=>resolve({type:'timeout'}),3000);
  }),origin);assert.equal(viewerDenied.type,'error','view permission is not transmit permission');
  const off=await api.post(`${origin}/api/live_streams/control`,{headers:managerHeaders,data:{...broadcast,comms_audio_enabled:false}});assert.equal(off.status(),200);
  const disabled=await (await api.get(audioUrl)).json();assert.equal(disabled.enabled,false);assert.equal(disabled.frames.length,0);
  await audience.waitForTimeout(500);const heard=await audience.evaluate(()=>programFrames);await audience.waitForTimeout(400);assert.equal(await audience.evaluate(()=>programFrames),heard,'disabling broadcast stops audience audio');
  await audience.screenshot({path:'/tmp/gs-audience-audio.png',fullPage:true});await spectator.close();
  await a.evaluate(()=>window.voiceSocket.close());await a.waitForFunction(()=>document.querySelector('#mic-state').textContent==='Microphone off');
  await b.waitForFunction(()=>document.querySelector('#count').textContent==='1 / 12');
  await b.click('#leave');assert.equal(await b.locator('#mic-state').textContent(),'Microphone off');
  // Bad credentials are rejected by the actual server.
  const denied=await a.evaluate(origin=>new Promise(resolve=>{
    const ws=new WebSocket(origin.replace('http','ws')+'/api/voice/ws');ws.onopen=()=>ws.send(JSON.stringify({type:'auth',token:'invalid'}));
    ws.onmessage=e=>{resolve(JSON.parse(e.data));ws.close();};setTimeout(()=>resolve({type:'timeout'}),3000);
  }),origin);assert.equal(denied.type,'error');
  assert.equal((await api.get(`${origin}/api/video/recordings`)).status(),403);
  assert.equal((await api.get(`${origin}/api/video/recordings`,{headers:{Authorization:'Bearer synthetic-viewer'}})).status(),403);
  const listing=await api.get(`${origin}/api/video/recordings`,{headers:{Authorization:'Bearer synthetic-producer'}});assert.equal(listing.status(),200);
  const clips=(await listing.json()).recordings;assert(clips.length>0,'create the MP4 fixture before running');
  const clip=clips.find(c=>!c.recently_updated);assert(clip);
  const part=await api.get(origin+clip.url,{headers:{Range:'bytes=0-31'}});assert.equal(part.status(),206);assert.equal((await part.body()).length,32);
  const other=clip.url.replace(clip.file,'2026-09-17_12-00-00-000002.mp4');assert.equal((await api.get(origin+other)).status(),401,'tickets cannot access another recording');
  await a.goto(`${origin}/media`);
  await a.evaluate(url=>{const v=document.querySelector('#recording-player');v.hidden=false;v.src=url;},clip.url);
  await a.waitForFunction(()=>document.querySelector('#recording-player').readyState>=2);
  assert.equal(errors.length,0,errors.join('\n'));
  await a.screenshot({path:'/tmp/gs-recordings-browser.png',fullPage:true});
  console.log('PASS: two clients, real PCM relay, PTT/open mic/mute/leave/disconnect, invalid auth, delayed listen-only audience, broadcast permissions/toggle, archive permissions, scoped tickets, range playback');
} catch (error) {
  for(const {page} of clients) console.error(await page.evaluate(()=>({sent:audioSent,received:audioReceived,status:document.querySelector('#status')?.textContent,mic:document.querySelector('#mic-state')?.textContent,peers:document.querySelector('#peers')?.textContent})).catch(()=>null));
  console.error(errors);throw error;
} finally {await browser.close();}
