// Fake devices and a local WebRTC peer; no hardware or running Ground Station needed.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
const {chromium} = await import(process.env.PLAYWRIGHT_MODULE || 'playwright');
const source = process.env.GS_MEDIA_SOURCE || path.resolve('backend/src');
const requests = [];
let permitted = true, receiver;
const server = http.createServer(async (req, res) => {
  requests.push({url:req.url, auth:req.headers.authorization, method:req.method});
  const url = new URL(req.url, 'http://localhost');
  const json = value => { res.setHeader('Content-Type','application/json'); res.end(JSON.stringify(value)); };
  if (url.pathname === '/') { res.end(`<iframe id="media" src="/media?embedded=1" allow="camera; microphone; autoplay"></iframe><iframe id="voice" src="/radio?embedded=1" allow="microphone; autoplay"></iframe><script>for(const frame of document.querySelectorAll('iframe'))frame.onload=()=>frame.contentWindow.postMessage({type:'gs26-session',token:'test-producer',visible:true},location.origin)</script>`); return; }
  const files = {'/media':'media.html','/radio':'voice.html','/assets/voice.js':'voice.js','/assets/media-login.js':'media-login.js','/assets/voice-worklet.js':'voice-worklet.js'};
  if (files[url.pathname]) { res.setHeader('Content-Type',url.pathname.endsWith('.js')?'text/javascript':'text/html'); res.end(fs.readFileSync(path.join(source,files[url.pathname]))); return; }
  if (url.pathname === '/api/live_streams') return json({can_manage_stream:permitted});
  if (url.pathname === '/api/video/streams' || url.pathname === '/api/stage-models') return json([]);
  if (url.pathname === '/api/video/recordings') return json({recordings:[],total:0,offset:0});
  if (url.pathname === '/api/voice/status') return json({can_transmit:true});
  if (url.pathname === '/api/video/publish' && req.method === 'POST') {
    let body=''; for await (const data of req) body+=data;
    if (!permitted) {res.statusCode=403;res.end('Permission denied');return;}
    assert.match(body, /H264\/90000/i); assert.doesNotMatch(body, /VP8\/90000/i);
    const answer = await receiver.evaluate(async sdp => {
      window.pc?.close(); window.pc = new RTCPeerConnection({iceServers:[]});
      await pc.setRemoteDescription({type:'offer',sdp}); await pc.setLocalDescription(await pc.createAnswer());
      await new Promise(resolve=> { if(pc.iceGatheringState==='complete')resolve();else pc.onicegatheringstatechange=()=>{if(pc.iceGatheringState==='complete')resolve();}; });
      return pc.localDescription.sdp;
    },body);
    res.statusCode=201; res.setHeader('Content-Type','application/sdp');res.setHeader('Location','/api/video/publish/browser-test/session-1');res.end(answer);return;
  }
  if (url.pathname.startsWith('/api/video/publish/')) {res.statusCode=204;res.end();return;}
  res.statusCode=404;res.end('Not found');
});
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
const origin=`http://127.0.0.1:${server.address().port}`;
const browser=await chromium.launch({headless:true,executablePath:process.env.CHROME_BINARY,args:['--use-fake-device-for-media-stream','--use-fake-ui-for-media-stream','--autoplay-policy=no-user-gesture-required']});
try {
  const context=await browser.newContext({permissions:['camera','microphone']});
  receiver=await context.newPage();await receiver.goto(origin+'/receiver');
  const page=await context.newPage();const errors=[];page.on('pageerror',e=>errors.push(e.message));
  await page.addInitScript(()=>{
    window.WebSocket=class {
      static OPEN=1;readyState=1;bufferedAmount=0;
      constructor(){setTimeout(()=>this.onopen?.(),0);}
      send(data){if(typeof data==='string'&&JSON.parse(data).type==='auth')setTimeout(()=>this.onmessage?.({data:JSON.stringify({type:'joined',id:1})}),0);}
      close(){this.readyState=3;}
    };
  });
  await page.goto(origin);
  const media=page.frameLocator('#media'), voice=page.frameLocator('#voice');
  await media.getByText('Connected with your dashboard session.',{exact:true}).waitFor();
  assert.equal(await media.locator('#login').isVisible(),false);
  assert.equal(await voice.locator('#login').isVisible(),false);
  await voice.locator('#join').click();await voice.getByText('Connected to crew voice.',{exact:true}).waitFor();
  await voice.locator('#mode').selectOption('open');
  await media.locator('#publish-start').click();
  await media.getByText('Sharing camera with the Ground Station',{exact:true}).waitFor({timeout:20000});
  const hide=()=>page.evaluate(()=>{for(const f of document.querySelectorAll('iframe')){f.style.display='none';f.contentWindow.postMessage({type:'gs26-session',token:'test-producer',visible:false},location.origin);}});
  await hide();
  const mediaFrame=page.frames().find(f=>f.url().includes('/media?'));
  const voiceFrame=page.frames().find(f=>f.url().includes('/radio?'));
  assert.equal(await mediaFrame.evaluate(()=>document.querySelector('#publish-preview').srcObject.getVideoTracks()[0].readyState),'live');
  assert.equal(await voiceFrame.evaluate(()=>document.querySelector('#mic-state').textContent),'Transmitting to crew');
  await page.evaluate(()=>document.querySelectorAll('iframe').forEach(f=>f.style.display='block'));
  await voice.locator('#mode').selectOption('ptt');
  const box = await voice.locator('#ptt').boundingBox();
  await page.mouse.move(box.x + 20, box.y + 20); await page.mouse.down();
  await hide();
  assert.equal(await voiceFrame.evaluate(()=>document.querySelector('#ptt').getAttribute('aria-pressed')),'false');
  await page.mouse.up();
  // Unrelated senders cannot replace the shared session.
  await mediaFrame.evaluate(()=>window.postMessage({type:'gs26-session',token:'attacker',visible:true},location.origin));
  await page.evaluate(()=>document.querySelectorAll('iframe').forEach(f=>f.style.display='block'));
  await media.locator('#publish-stop').click();assert.equal(await media.locator('#publish-preview').isVisible(),false);
  await media.locator('#publish-start').click();await media.getByText('Sharing camera with the Ground Station',{exact:true}).waitFor({timeout:20000});
  permitted=false;
  await media.locator('#publish-preview').waitFor({state:'hidden',timeout:10000});
  assert.equal(await media.locator('#publisher').isVisible(),false);
  await page.evaluate(()=>{for(const f of document.querySelectorAll('iframe'))f.contentWindow.postMessage({type:'gs26-session',token:'',visible:true},location.origin);});
  await voice.getByText('Sign in from the dashboard to join crew voice.',{exact:true}).waitFor();
  assert.equal(await voice.locator('#mic-state').textContent(),'Microphone off');
  assert(!requests.some(r=>r.url.startsWith('/api/auth/')),'embedded tools never log in again');
  assert(requests.filter(r=>r.url.startsWith('/api/video/publish')).every(r=>r.auth==='Bearer test-producer'),'publishing always uses shared session');
  assert(requests.some(r=>r.method==='DELETE'&&r.url.startsWith('/api/video/publish/')),'stop deletes relay session');
  assert.deepEqual(errors,[]);
  console.log('PASS: shared login, fake-camera WebRTC publishing, hidden-tab persistence, open mic, PTT release, stop, permission loss, logout and sender validation');
} finally {await browser.close();await new Promise(resolve=>server.close(resolve));}
