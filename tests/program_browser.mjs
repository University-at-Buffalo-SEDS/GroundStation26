// Requires Playwright, Chrome and FFmpeg. Uses synthetic HLS, no station or MediaMTX.
import assert from 'node:assert/strict';
import {readFileSync,mkdtempSync,rmSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {execFileSync} from 'node:child_process';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const dir=mkdtempSync(join(tmpdir(),'gs-program-test-'));
let browser;
try {
  execFileSync('ffmpeg',['-hide_banner','-loglevel','error','-f','lavfi','-i','testsrc2=size=640x360:rate=10','-t','120','-an','-c:v','libx264','-preset','ultrafast','-profile:v','baseline','-pix_fmt','yuv420p','-g','10','-keyint_min','10','-sc_threshold','0','-f','hls','-hls_time','1','-hls_list_size','0','-hls_segment_type','fmp4','-hls_segment_filename',join(dir,'seg%d.m4s'),join(dir,'index.m3u8')]);
  browser=await chromium.launch({headless:true,executablePath:process.env.CHROME_BINARY,args:['--autoplay-policy=no-user-gesture-required']});
  const page=await browser.newPage({viewport:{width:360,height:800}});
  const errors=[];page.on('pageerror',e=>errors.push(e.message));
  const start=Date.now()-20000;
  let live=true,offline=false,layout='hero',delay=3,audioCursor=0;
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
    if(path==='/assets/program-audio.js')return route.fulfill({contentType:'text/javascript',body:readFileSync('backend/src/media/program-audio.js')});
    if(path==='/assets/three/vehicle-renderer.js')return route.fulfill({contentType:'text/javascript',body:"customElements.define('gs-vehicle-viewer',class extends HTMLElement{});"});
    if(path==='/api/media-assets/program/state') {
      if(offline)return route.fulfill({status:503,body:'offline'});
      return route.fulfill({json:{server_now_ms:Date.now(),broadcast:{delay_seconds:delay,comms_audio_enabled:true,label:'Test mission',featured_stream_id:'camera',layout},streams:live?['camera','other'].map(id=>({id,label:id,url:`/hls/${id}/index.m3u8`})):[],telemetry:{phase:'Idle',t_clock:'T− 00:10.00',stats:[{label:'Altitude',value:12,precision:1,unit:'m'}]}}});
    }
    if(path==='/api/media-assets/program/audio')return route.fulfill({json:{enabled:true,generation:1,cursor:++audioCursor,frames:[{id:1,pcm:Buffer.alloc(640).toString('base64')}]}});
    if(path.endsWith('index.m3u8')){
      const end=Math.floor((Date.now()-start)/1000)-delay;
      const lines=['#EXTM3U','#EXT-X-VERSION:7','#EXT-X-TARGETDURATION:1','#EXT-X-MEDIA-SEQUENCE:0','#EXT-X-MAP:URI="init.mp4"'];
      for(let i=0;i<end;i++)lines.push(`#EXT-X-PROGRAM-DATE-TIME:${new Date(start+i*1000).toISOString()}`,'#EXTINF:1,',`seg${i}.m4s`);
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
  await page.setViewportSize({width:360,height:800});layout='grid';
  await page.waitForFunction(()=>document.querySelectorAll('video').length===2&&[...document.querySelectorAll('video')].every(v=>v.style.opacity==='1'));
  assert.deepEqual(await page.locator('video').evaluateAll(v=>v.map(x=>x.style.width)),['100%','100%'],'portrait grid stacks cameras');
  assert.equal(await page.locator('#model-stage').isVisible(),false,'camera grids do not show a 3D model by default');
  layout='grid-model';
  await page.waitForFunction(()=>document.querySelector('#model-stage').style.display==='block'&&document.querySelector('#model-stage').style.height.includes('33.3'));
  assert.equal(await page.locator('video').count(),2,'model tile coexists with the camera feeds');
  assert.equal(await page.locator('#model-stage gs-vehicle-viewer').count(),1);
  layout='model';
  await page.waitForFunction(()=>!document.querySelector('video')&&document.querySelector('#model-stage').style.height==='100%');
  assert.equal(await page.locator('#model-stage').isVisible(),true,'operator may select a model-only view');
  layout='hero';
  await page.waitForFunction(()=>document.querySelectorAll('video').length===1&&document.querySelector('video').style.opacity==='1');
  assert.equal(await page.locator('#model-stage').isVisible(),false);
  delay=10;await page.waitForTimeout(1600);
  await page.waitForFunction(()=>[...document.querySelectorAll('video')].some(v=>v.style.opacity==='1'));
  live=false;await page.waitForFunction(()=>!document.querySelector('video')&&document.querySelector('#model-stage').style.display==='block');await page.locator('#rocket-inset').waitFor({state:'visible'});
  offline=true;await page.waitForFunction(()=>document.querySelector('#status').textContent.includes('unavailable'));
  await page.locator('#rocket-inset').waitFor({state:'visible'});
  assert.equal(errors.length,0,errors.join('\n'));
  console.log('PASS: H.264 playback advances, single decoder, portrait grid/status bar, unified camera/crew sound without worklet or microphone, delay changes, camera loss and server failure');
} finally {await browser?.close();rmSync(dir,{recursive:true,force:true});}
