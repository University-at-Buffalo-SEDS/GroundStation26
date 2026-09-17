// One sound control for camera audio and delayed, receive-only crew comms.
// AudioBuffer playback also works on LAN HTTP where AudioWorklet is unavailable.
const controls=document.querySelector('#stream-sound');
const button=document.createElement('button');button.textContent='Unmute stream';button.setAttribute('aria-pressed','false');
const volume=document.createElement('input');volume.type='range';volume.min='0';volume.max='100';volume.value='100';volume.setAttribute('aria-label','Stream volume');
const label=document.createElement('span');label.id='audio-status';label.setAttribute('role','status');
controls.append(button,volume,label);
let context,gain,listening=false,stopped=false,cursor=0,generation=null,requestGeneration=0,program=null,lastState=0,busy=false,wantSound=true;
const speakers=new Map(),sources=new Set();
const ticket=new URLSearchParams(location.search).get('ticket');
function reset(){cursor=0;requestGeneration++;for(const source of sources){source.stop();source.disconnect();}sources.clear();speakers.clear();}
function settings(){
  const level=Number(volume.value)/100;
  if(gain)gain.gain.value=level;
  window.gsStreamSound={muted:!listening,volume:level};
  window.dispatchEvent(new Event('gs-stream-sound'));
  button.textContent=listening?'Mute stream':'Unmute stream';button.setAttribute('aria-pressed',String(listening));
}
function stateUpdate(next){
  if(!next)return;
  if(program?.delay!==next.delay||program?.enabled!==next.enabled||!next.fresh)reset();
  program=next;lastState=performance.now();
  label.textContent=listening&&next.enabled?(next.fresh?'Crew comms included':'Audio paused · program unavailable'):'';
}
window.addEventListener('gs-program-audio-state',event=>stateUpdate(event.detail));
stateUpdate(window.gsProgramAudioState);
async function enable(){
  if(busy||listening||stopped)return;
  busy=true;
  try{
    if(!context){context=new AudioContext();gain=context.createGain();gain.connect(context.destination);context.onstatechange=()=>{if(wantSound&&context.state==='running'&&!listening&&!stopped){listening=true;reset();settings();stateUpdate(program);}};}
    // Do not wait on resume during autoplay: some browsers keep it pending until a gesture.
    const resumed=context.resume();resumed.catch(()=>{});
    if(context.state!=='running')return;
    listening=true;reset();settings();stateUpdate(program);
  }catch(error){label.textContent=`Cannot enable stream sound: ${error.message}`;}
  finally{busy=false;}
}
button.onclick=async()=>{
  if(listening){wantSound=false;listening=false;reset();settings();await context?.suspend();label.textContent='';}
  else{
    wantSound=true;await enable();
    // resume can finish asynchronously even after a valid user gesture.
    if(!listening&&context){await context.resume();await enable();}
  }
};
volume.oninput=settings;
function play(frame){
  if(context?.state!=='running')return;
  const bytes=Uint8Array.from(atob(frame.pcm),c=>c.charCodeAt(0));
  if(bytes.length!==640)return;
  const pcm=new DataView(bytes.buffer),buffer=context.createBuffer(1,320,16000),samples=buffer.getChannelData(0);
  for(let i=0;i<320;i++)samples[i]=pcm.getInt16(i*2,true)/32768;
  const now=context.currentTime;
  // Bound queued playback per speaker; mix simultaneous speakers on the same clock.
  const prior=speakers.get(frame.id)||0;
  if(prior>now+.35)return;
  const at=Math.max(now+.04,prior);
  const source=context.createBufferSource();source.buffer=buffer;source.connect(gain);sources.add(source);
  source.onended=()=>{sources.delete(source);source.disconnect();};source.start(at);speakers.set(frame.id,at+.02);
  for(const [id,end] of speakers)if(end<now-1)speakers.delete(id);
}
async function poll(){
  if(stopped)return;
  if(listening&&program?.enabled&&program.fresh&&performance.now()-lastState<2500){
    const current=requestGeneration;
    try{
      const response=await fetch(`/api/media-assets/program/audio?ticket=${encodeURIComponent(ticket)}&after=${cursor}`,{cache:'no-store'});
      if(!response.ok)throw Error('Stream audio unavailable');
      const batch=await response.json();
      if(current!==requestGeneration||!listening||stopped)return;
      if(!batch.enabled){reset();label.textContent='';return;}
      if(generation!==batch.generation){reset();generation=batch.generation;}
      cursor=batch.cursor;
      for(const frame of batch.frames)play(frame);
      label.textContent='Crew comms included';
    }catch(error){reset();label.textContent=error.message;}
    finally{if(!stopped)setTimeout(poll,100);}
    return;
  }
  if(listening&&performance.now()-lastState>=2500){reset();label.textContent='Audio paused · program unavailable';}
  setTimeout(poll,100);
}
// Start sound with the program wherever autoplay is allowed. Otherwise one stream
// unmute gesture enables both camera sound and comms, without joining the call.
enable();poll();
window.addEventListener('pagehide',()=>{stopped=true;listening=false;reset();context?.close();});
