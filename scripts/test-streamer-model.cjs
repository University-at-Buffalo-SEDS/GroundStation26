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
 const result=await evaluate("(()=>{clearInterval(timer);stopped=true;state.telemetry={phase:'Ascent'};lastPoll=performance.now();modelDisplay(true);return {corner:document.querySelector('#rocket-inset').style.display,full:document.querySelector('#model-stage').style.display,flame:document.querySelector('#flame').style.display,phase:document.querySelector('#rocket-phase').textContent};})()");
 assert.equal(result.corner,'block');assert.equal(result.full,'none');assert.equal(result.flame,'');assert.equal(result.phase,'Ascent');
 assert.equal(await evaluate("(()=>{state.telemetry=null;modelDisplay(true);return document.querySelector('#rocket-phase').textContent;})()"),'Unknown');
 console.log('PASS: offline full-size model loads; video-mode 2D corner uses delayed phase; absent delayed state stays unknown');
 ws.close();
})().catch(e=>{console.error(e);process.exit(1);});
