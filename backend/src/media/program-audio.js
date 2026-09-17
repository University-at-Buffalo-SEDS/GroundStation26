// Receive-only audience audio. Never creates a microphone stream or a transmit socket.
const controls=document.createElement('div');
controls.style.cssText='position:absolute;left:14px;top:48px;z-index:80;padding:8px;background:#080d15e8;border-radius:8px;display:flex;gap:10px;align-items:center;flex-wrap:wrap;max-width:75%';
const button=document.createElement('button');button.textContent='Enable crew audio';button.disabled=true;
const volume=document.createElement('input');volume.type='range';volume.min='0';volume.max='150';volume.value='100';volume.setAttribute('aria-label','Crew audio volume');
const label=document.createElement('span');label.textContent='Crew audio is not in this broadcast';label.setAttribute('role','status');
controls.append(button,volume,label);document.body.append(controls);
let context,node,listening=false,stopped=false,cursor=0,generation=null,requestGeneration=0,program=null,lastState=0;
const ticket=new URLSearchParams(location.search).get('ticket');
function reset(){cursor=0;requestGeneration++;node?.port.postMessage({type:'reset'});}
function settings(){node?.port.postMessage({type:'settings',transmit:false,gain:0,volume:Number(volume.value)/100});}
function stateUpdate(next){
  if(!next)return;
  if(program?.delay!==next.delay||program?.enabled!==next.enabled||!next.fresh)reset();
  program=next;lastState=performance.now();button.disabled=!next.enabled||!next.fresh;
  if(!next.enabled){label.textContent='Crew audio is not in this broadcast';}
  else if(!next.fresh){label.textContent='Crew audio paused · program unavailable';}
  else if(!listening){label.textContent='Listen only · click to enable delayed crew audio';}
}
window.addEventListener('gs-program-audio-state',event=>stateUpdate(event.detail));
stateUpdate(window.gsProgramAudioState);
button.onclick=async()=>{
  if(listening){listening=false;reset();await context?.suspend();button.textContent='Enable crew audio';label.textContent='Crew audio muted';return;}
  try{
    if(!context){context=new AudioContext();await context.audioWorklet.addModule('/assets/voice-worklet.js');node=new AudioWorkletNode(context,'crew-audio',{numberOfInputs:0,numberOfOutputs:1,outputChannelCount:[1]});node.connect(context.destination);}
    await context.resume();listening=true;reset();settings();button.textContent='Mute crew audio';label.textContent='Listening to delayed crew audio';
  }catch(error){listening=false;node?.disconnect();await context?.close().catch(()=>{});context=null;node=null;label.textContent=`Cannot enable audio: ${error.message}`;}
};
volume.oninput=settings;
async function poll(){
  if(stopped)return;
  if(listening&&program?.enabled&&program.fresh&&performance.now()-lastState<2500){
    const current=requestGeneration;
    try{
      const response=await fetch(`/api/media-assets/program/audio?ticket=${encodeURIComponent(ticket)}&after=${cursor}`,{cache:'no-store'});
      if(!response.ok)throw Error('Crew audio unavailable');
      const batch=await response.json();
      if(current!==requestGeneration||!listening||stopped)return;
      if(!batch.enabled){reset();label.textContent='Crew audio removed from broadcast';return;}
      if(generation!==batch.generation){node.port.postMessage({type:'reset'});generation=batch.generation;}
      cursor=batch.cursor;
      for(const frame of batch.frames){const buffer=Uint8Array.from(atob(frame.pcm),c=>c.charCodeAt(0)).buffer;node.port.postMessage({type:'audio',id:frame.id,buffer},[buffer]);}
      label.textContent='Listening to delayed crew audio';
    }catch(error){reset();label.textContent=error.message;}
    finally{if(!stopped)setTimeout(poll,100);}
    return;
  }
  if(listening&&performance.now()-lastState>=2500){reset();label.textContent='Crew audio paused · program unavailable';}
  setTimeout(poll,100);
}
poll();
window.addEventListener('pagehide',()=>{stopped=true;listening=false;reset();node?.disconnect();context?.close();});
