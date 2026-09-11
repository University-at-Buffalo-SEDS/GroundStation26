// Requires the isolated media fixture and Chromium CDP described in broadcast-studio.md.
// Run with no camera relay on 19997 to exercise the outage/model fallback.
const assert=require('node:assert/strict');
const wait=ms=>new Promise(r=>setTimeout(r,ms));
(async()=>{
 const cfg=await(await fetch('http://127.0.0.1:19090/api/live_streams')).json();
 const pages=await(await fetch('http://127.0.0.1:19101/json')).json();
 const ws=new WebSocket(pages.find(p=>p.type==='page').webSocketDebuggerUrl);await new Promise(r=>ws.onopen=r);
 let id=0;const pending=new Map();ws.onmessage=e=>{const m=JSON.parse(e.data);if(m.id){pending.get(m.id)(m);pending.delete(m.id);}};
 const call=(method,params={})=>new Promise(r=>{pending.set(++id,r);ws.send(JSON.stringify({id,method,params}));});
 const evaluate=async expression=>{const m=await call('Runtime.evaluate',{expression,returnByValue:true});assert.ok(!m.result.exceptionDetails,JSON.stringify(m.result.exceptionDetails));return m.result.result.value;};
 await call('Page.navigate',{url:'http://127.0.0.1:19090'+cfg.program_url});
 let ready=false;for(let i=0;i<100;i++){await wait(250);ready=await evaluate("document.querySelector('#model-stage gs-vehicle-viewer')?.dataset.loaded==='true'");if(ready)break;}
 assert.ok(ready,'Full-size fallback GLB should load without a camera');
 assert.equal(await evaluate("state.streams.length"),0);
 assert.equal(await evaluate("state.model.stages[0].id"),'stage-1');
 assert.equal(await evaluate("state.model.ground_model_url"),'/assets/models/gse-site.glb');
 await evaluate("clearInterval(timer);stopped=true;state.telemetry={phase:'PreFill'};lastPoll=performance.now();modelDisplay(false)");
 let ground=false;
 for(let i=0;i<80;i++){await wait(250);ground=await evaluate("!!document.querySelector('#model-stage gs-vehicle-viewer')?.model?.getObjectByName('fill_manifold')");if(ground)break;}
 assert.ok(ground,'Prelaunch fallback includes real fill equipment');
 const groundInset=await evaluate("(()=>{lastPoll=performance.now();modelDisplay(true);return {visible:getComputedStyle(document.querySelector('#rocket-inset')).display,equipment:getComputedStyle(document.querySelector('#ground-equipment')).display,stack:getComputedStyle(document.querySelector('#stage')).isolation};})()");
 assert.equal(groundInset.visible,'block');
 assert.notEqual(groundInset.equipment,'none');
 assert.equal(groundInset.stack,'isolate');
 await evaluate("state.telemetry={phase:'Launch'};lastPoll=performance.now();modelDisplay(false)");
 let flight=false;
 for(let i=0;i<80;i++){await wait(250);flight=await evaluate("(()=>{const m=document.querySelector('#model-stage gs-vehicle-viewer')?.model;return !!m?.getObjectByName('stage-1')&&!m.getObjectByName('fill_manifold')})()");if(flight)break;}
 assert.ok(flight,'Launch removes the pad, tower and tanks');
 assert.equal(await evaluate("getComputedStyle(document.querySelector('#ground-equipment')).display"),'none');
 const result=await evaluate("(()=>{clearInterval(timer);stopped=true;state.telemetry={phase:'Ascent'};lastPoll=performance.now();modelDisplay(true);return {corner:document.querySelector('#rocket-inset').style.display,full:document.querySelector('#model-stage').style.display,flame:document.querySelector('#flame').style.display,phase:document.querySelector('#rocket-phase').textContent};})()");
 assert.equal(result.corner,'block');assert.equal(result.full,'none');assert.equal(result.flame,'');assert.equal(result.phase,'Ascent');
 assert.equal(await evaluate("(()=>{state.telemetry=null;modelDisplay(true);return document.querySelector('#rocket-phase').textContent;})()"),'Unknown');
 console.log('PASS: offline model loads; delayed prelaunch renders equipment; launch removes it; video-mode 2D corner uses delayed phase; absent state stays unknown');
 ws.close();
})().catch(e=>{console.error(e);process.exit(1);});
