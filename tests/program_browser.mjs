// Requires Playwright, Chrome and FFmpeg. Uses synthetic HLS, no station or MediaMTX.
import assert from 'node:assert/strict';
import {readFileSync,mkdtempSync,rmSync,cpSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {execFileSync} from 'node:child_process';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const dir=mkdtempSync(join(tmpdir(),'gs-program-test-'));
let browser;
try {
  if(process.env.GS_PROGRAM_TEST_MEDIA) cpSync(process.env.GS_PROGRAM_TEST_MEDIA,dir,{recursive:true});
  else execFileSync('ffmpeg',['-hide_banner','-loglevel','error','-f','lavfi','-i','testsrc2=size=640x360:rate=10','-t','120','-an','-c:v','libx264','-preset','ultrafast','-profile:v','baseline','-pix_fmt','yuv420p','-g','10','-keyint_min','10','-sc_threshold','0','-f','hls','-hls_time','1','-hls_list_size','0','-hls_segment_type','fmp4','-hls_segment_filename',join(dir,'seg%d.m4s'),join(dir,'index.m3u8')]);
  browser=await chromium.launch({headless:true,executablePath:process.env.CHROME_BINARY,args:['--autoplay-policy=no-user-gesture-required']});
  const page=await browser.newPage({viewport:{width:360,height:800}});
  const errors=[];page.on('pageerror',e=>errors.push(e.message));
  const start=Date.now()-20000;
  let live=true,offline=false,layout='hero',delay=3,audioCursor=0,replay=null,live3dRequests=0;
  await page.addInitScript(()=>{
    window.audioFrames=0;
    const start=AudioBufferSourceNode.prototype.start;
    AudioBufferSourceNode.prototype.start=function(...args){window.audioFrames++;return start.apply(this,args);};
    navigator.mediaDevices.getUserMedia=()=>{throw Error('Audience must never request microphone');};
    window.AudioWorkletNode=undefined; // Native/LAN receive playback must not need worklets.
  });
  await page.route('**/*',async route=>{
    const path=new URL(route.request().url()).pathname;
    if(path==='/program')return route.fulfill({contentType:'text/html',body:readFileSync('backend/src/media/program.html')});
    if(path==='/assets/hls.min.js')return route.fulfill({contentType:'text/javascript',body:readFileSync('backend/assets/hls.min.js')});
    if(path==='/assets/program-art.js')return route.fulfill({contentType:'text/javascript',body:readFileSync('backend/src/media/program-art.js')});
    if(path==='/assets/program-audio.js')return route.fulfill({contentType:'text/javascript',body:readFileSync('backend/src/media/program-audio.js')});
    if(path.startsWith('/assets/three/')){live3dRequests++;return route.abort();}
    if(path==='/api/media-assets/program/state') {
      if(offline)return route.fulfill({status:503,body:'offline'});
      return route.fulfill({json:{server_now_ms:Date.now(),broadcast:{replay:replay&&Date.now()<replay.started_at_ms+replay.duration_seconds*1000/replay.speed?replay:null,delay_seconds:delay,comms_audio_enabled:true,label:'Test mission',featured_stream_id:'camera',layout},streams:live?(replay&&Date.now()<replay.started_at_ms+replay.duration_seconds*1000/replay.speed?replay.cameras.map(c=>c.id):['camera','other']).map(id=>({id,label:id,url:`/hls/${id}/index.m3u8`})):[],telemetry:{phase:'Idle',t_clock:'T− 00:10.00',stats:[{label:'Altitude',value:12,precision:1,unit:'m'}]}}});
    }
    if(path==='/api/media-assets/program/audio')return route.fulfill({json:{enabled:true,generation:1,cursor:++audioCursor,frames:[{id:1,pcm:Buffer.alloc(640).toString('base64')}]}});
    if(path.endsWith('index.m3u8')){
      const frozen=replay&&Date.now()<replay.started_at_ms+replay.duration_seconds*1000/replay.speed;
      const first=frozen?Math.max(0,Math.floor((replay.source_end_ms-replay.duration_seconds*1000-start)/1000)):0;
      const end=frozen?Math.ceil((replay.source_end_ms-start)/1000):Math.floor((Date.now()-start)/1000)-delay;
      const lines=['#EXTM3U','#EXT-X-VERSION:7','#EXT-X-TARGETDURATION:1',`#EXT-X-MEDIA-SEQUENCE:${first}`,'#EXT-X-MAP:URI="init.mp4"'];
      for(let i=first;i<end;i++)lines.push(`#EXT-X-PROGRAM-DATE-TIME:${new Date(start+i*1000).toISOString()}`,'#EXTINF:1,',`seg${i}.m4s`);
      if(frozen)lines.push('#EXT-X-ENDLIST');
      return route.fulfill({contentType:'application/vnd.apple.mpegurl',body:lines.join('\n')+'\n'});
    }
    const file=path.split('/').pop();
    if(/^(init\.mp4|seg\d+\.m4s)$/.test(file))return route.fulfill({contentType:'video/mp4',body:readFileSync(join(dir,file))});
    return route.fulfill({status:404,body:''});
  });
  await page.goto('http://localhost/program?ticket=synthetic');
  await page.waitForFunction(()=>[...document.querySelectorAll('video')].some(v=>v.style.opacity==='1'&&v.readyState>=2),{},{timeout:20000});
  assert.equal(await page.locator('video').count(),1,'hero only decodes one camera');
  await page.waitForFunction(()=>audioFrames>3);
  assert.equal(await page.getByRole('button',{name:'Enable crew audio',exact:true}).count(),0);
  await page.getByRole('button',{name:'Mute stream',exact:true}).click();
  assert(await page.locator('video').evaluate(v=>v.muted),'one mute controls camera sound');
  const muted=await page.evaluate(()=>audioFrames);await page.waitForTimeout(350);assert.equal(await page.evaluate(()=>audioFrames),muted);
  await page.getByRole('button',{name:'Unmute stream',exact:true}).click();await page.waitForFunction(n=>audioFrames>n,muted);
  const before=await page.locator('video').evaluate(v=>v.currentTime);await page.waitForTimeout(2200);
  assert((await page.locator('video').evaluate(v=>v.currentTime))>before+1,'video advances, not stuck in a seek loop');
  for(const size of [{width:320,height:900},{width:400,height:700},{width:1024,height:600}]){
    await page.setViewportSize(size);await page.waitForTimeout(200);
    const boxes=await page.evaluate(()=>{
      const box=id=>{const r=document.querySelector(id).getBoundingClientRect();return {x:r.x,y:r.y,width:r.width,height:r.height,bottom:r.bottom,right:r.right};};
      return {rocket:box('#rocket-inset'),bar:box('#program-bar'),view:box('#viewport'),width:document.documentElement.scrollWidth};
    });
    assert(boxes.rocket.y>=boxes.bar.y&&boxes.rocket.bottom<=boxes.bar.bottom,'rocket stays in status bar');
    assert(boxes.bar.y>=boxes.view.bottom-1,'status bar does not cover video');
    assert(boxes.width<=size.width,'no horizontal overflow');
  }
  await page.setViewportSize({width:360,height:800});
  // The second camera was not on air in hero mode; replay selects it first.
  replay={id:'replay-one',duration_seconds:6,speed:0.25,started_at_ms:Date.now(),source_end_ms:Date.now()-delay*1000-2500,cameras:[{id:'other',x:20,y:70,zoom:2},{id:'camera',x:50,y:50,zoom:1}]};
  await page.waitForFunction(()=>document.querySelectorAll('video').length===2&&[...document.querySelectorAll('video')].every(v=>v.playbackRate===0.25&&v.style.opacity==='1'));
  assert.match(await page.locator('#banner').innerText(),/Replay/i);
  const second=page.locator('video[aria-label="other"]');
  assert.equal(await second.evaluate(v=>v.style.transform),'scale(2)');
  const clipBefore=await second.evaluate(v=>v.currentTime);await page.waitForTimeout(2400);
  const advanced=(await second.evaluate(v=>v.currentTime))-clipBefore;
  assert(advanced>0.2&&advanced<1.5,'quarter-speed video advances without chasing live time: '+advanced);
  replay.cameras.reverse();replay.cameras.find(c=>c.id==='other').zoom=2.5;
  await page.waitForFunction(()=>document.querySelector('video[aria-label="other"]').style.transform==='scale(2.5)');
  const positions=await page.locator('video').evaluateAll(vs=>Object.fromEntries(vs.map(v=>[v.getAttribute('aria-label'),v.parentElement.style.top])));
  assert.equal(positions.camera,'0%');assert.equal(positions.other,'50%','reorder changes existing decoder tiles');
  replay.started_at_ms=Date.now()-30000;
  await page.waitForFunction(()=>document.querySelectorAll('video').length===1&&document.querySelector('video').playbackRate===1&&document.querySelector('video').style.opacity==='1');
  replay=null;
  await page.setViewportSize({width:360,height:800});layout='grid';
  await page.waitForFunction(()=>document.querySelectorAll('video').length===2&&[...document.querySelectorAll('video')].every(v=>v.style.opacity==='1'));
  assert.deepEqual(await page.locator('video').evaluateAll(v=>v.map(x=>x.style.width)),['100%','100%'],'portrait grid stacks cameras');
  assert.equal(await page.locator('#graphic-stage').isVisible(),false,'camera grids do not show a 3D model by default');
  layout='grid-model';
  await page.waitForFunction(()=>document.querySelector('#graphic-stage').style.display==='flex'&&document.querySelector('#graphic-stage').style.height.includes('33.3'));
  assert.equal(await page.locator('video').count(),2,'2D graphic tile coexists with the camera feeds');
  await page.locator('#graphic-stage #rocket-inset svg').waitFor({state:'visible'});
  assert.equal(await page.locator('#graphic-stage gs-vehicle-viewer').count(),0,'live graphic uses no 3D viewer');
  layout='model';
  await page.waitForFunction(()=>!document.querySelector('video')&&document.querySelector('#graphic-stage').style.height==='100%');
  assert.equal(await page.locator('#graphic-stage').isVisible(),true,'operator may select a 2D graphic-only view');
  layout='hero';
  await page.waitForFunction(()=>document.querySelectorAll('video').length===1&&document.querySelector('video').style.opacity==='1');
  assert.equal(await page.locator('#graphic-stage').isVisible(),false);
  delay=10;await page.waitForTimeout(1600);
  await page.waitForFunction(()=>[...document.querySelectorAll('video')].some(v=>v.style.opacity==='1'));
  live=false;await page.waitForFunction(()=>!document.querySelector('video')&&document.querySelector('#graphic-stage').dataset.flat==='true');await page.locator('#rocket-inset').waitFor({state:'visible'});
  assert.equal(await page.locator('#graphic-stage #rocket-inset').count(),1,'2D rocket fills the main area without a camera');
  assert.equal(await page.locator('#graphic-stage gs-vehicle-viewer').count(),0,'fallback needs no 3D viewer');
  live=true;await page.waitForFunction(()=>document.querySelector('video')?.style.opacity==='1');
  assert.equal(await page.locator('#program-bar #rocket-inset').count(),1,'same artwork returns beside the live camera');
  offline=true;await page.waitForFunction(()=>document.querySelector('#status').textContent.includes('unavailable'));
  await page.locator('#rocket-inset').waitFor({state:'visible'});
  assert.equal(live3dRequests,0,'3D assets are reserved for saved replay');
  assert.equal(errors.length,0,errors.join('\n'));
  console.log('PASS: H.264 playback advances, single decoder, portrait grid/status bar, unified camera/crew sound without worklet or microphone, delay changes, camera loss and server failure');
} finally {await browser?.close();rmSync(dir,{recursive:true,force:true});}
