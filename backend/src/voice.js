import {signIn} from '/assets/media-login.js';
const $=id=>document.getElementById(id);
let token='', session=null, muted=false, held=false, joining=false, radioAvailable=false;
const noiseSupported=navigator.mediaDevices?.getSupportedConstraints?.().noiseSuppression===true;
$('noise-suppression').disabled=!noiseSupported;
if(!noiseSupported){$('noise-suppression').checked=false;$('noise-status').textContent='Noise suppression is unavailable in this browser.';}
function radioLink(radio){
  $('radio-controls').hidden=!radio?.configured;
  radioAvailable=radio?.available===true;
  $('radio-enable').disabled=!session?.ready||!radioAvailable;
  $('radio-status').textContent=radioAvailable?'Recovery radio ready · 8 kb/s push-to-talk':'Recovery radio unavailable · local voice still works';
  if(!radioAvailable){$('radio-enable').checked=false;if(session)session.radio=false;}
}
const notice=text=>{$('status').textContent=text;};
async function bounded(promise,ms,message){
 let timer;try{return await Promise.race([promise,new Promise((_,reject)=>{timer=setTimeout(()=>reject(new Error(message)),ms);})]);}finally{clearTimeout(timer);}
}
function updateControls(){
  $('join').disabled=!token||!!session||joining;$('leave').disabled=!session;
  $('device').disabled=!!session||joining;$('mode').disabled=joining;
  $('mute').disabled=!session?.ready||$('mode').value==='off';$('ptt').disabled=!session?.ready||muted||$('mode').value!=='ptt';
  $('logout').disabled=!token;$('signin').disabled=!!session||joining;
  $('radio-enable').disabled=!session?.ready||!radioAvailable;
  $('noise-suppression').disabled=!noiseSupported||joining||!!session?.reconfiguring;
}
function transmit(){
  const mode=$('mode').value;
  const enabled=!!session?.ready&&!session.reconfiguring&&!muted&&(mode==='open'||(mode==='ptt'&&held));
  if(session){
    if(session.transmitting!==enabled){
      session.transmitting=enabled;
      if(session.ws?.readyState===WebSocket.OPEN)session.ws.send(JSON.stringify({type:'transmitting',enabled}));
    }
    const radio=$('radio-enable').checked&&$('mode').value==='ptt';
    if(session.radio!==radio&&session.ws?.readyState===WebSocket.OPEN){session.radio=radio;session.ws.send(JSON.stringify({type:'radio',enabled:radio}));}
    session.stream?.getAudioTracks().forEach(t=>{t.enabled=enabled;});
    session.node?.port.postMessage({type:'settings',transmit:enabled,gain:Number($('gain').value)/100,volume:Number($('volume').value)/100});
  }
  $('ptt').setAttribute('aria-pressed',String(enabled&&$('mode').value==='ptt'));
  $('mute').setAttribute('aria-pressed',String(muted));$('mute').textContent=muted?'Unmute microphone':'Mute microphone';
  $('mic-state').textContent=!session?'Microphone off':session.reconfiguring?'Updating microphone…':enabled?'Transmitting to crew':mode==='off'?'Microphone off · listening':muted?'Microphone muted':'Listening · hold to talk';
  updateControls();
}
function leave(message='Left voice. Microphone released.'){
  const old=session;session=null;held=false;joining=false;$('radio-enable').checked=false;
  if(old){clearInterval(old.ping);clearTimeout(old.joinTimeout);old.ws?.close();old.stream?.getTracks().forEach(t=>t.stop());old.node?.disconnect();old.context?.close().catch(()=>{});}
  $('peers').replaceChildren();$('count').textContent='0 / 12';$('level').value=0;transmit();notice(message);
}
async function devices(){
  try{
    const selected=$('device').value;
    const list=await navigator.mediaDevices.enumerateDevices();$('device').replaceChildren(new Option('System default',''));
    list.filter(d=>d.kind==='audioinput').forEach((d,i)=>$('device').add(new Option(d.label||`Microphone ${i+1}`,d.deviceId)));
    if([...$('device').options].some(o=>o.value===selected))$('device').value=selected;
  }catch(e){notice(`Cannot list microphones: ${e.message}`);}
}
$('login').onsubmit=async event=>{
  event.preventDefault();$('signin').disabled=true;
  const password=$('password').value;$('password').value='';
  try{const result=await signIn($('username').value,password);token=result.token;notice('Signed in. Choose Join voice to enable your microphone.');}
  catch(e){notice(e.message);}finally{updateControls();}
};
$('logout').onclick=async()=>{
  leave();const old=token;token='';updateControls();
  try{await fetch('/api/auth/logout',{method:'POST',headers:{Authorization:`Bearer ${old}`}});}finally{notice('Signed out.');}
};
$('join').onclick=async()=>{
  if(session||joining)return;
  if(!window.isSecureContext||!navigator.mediaDevices?.getUserMedia)return notice('Microphone access requires HTTPS (or localhost).');
  joining=true;muted=false;held=false;updateControls();notice('Starting voice audio…');
  const s={ready:false,transmitting:false};session=s;
  try{
    const Context=window.AudioContext||window.webkitAudioContext;
    if(!Context)throw new Error('This browser does not support voice audio');
    s.context=new Context();
    await bounded(s.context.resume(),8000,'Audio could not start. Allow audio playback, then join again.');
    if(!s.context.audioWorklet)throw new Error('Voice audio requires a trusted HTTPS connection and AudioWorklet support');
    await bounded(s.context.audioWorklet.addModule('/assets/voice-worklet.js'),8000,'The voice audio processor did not load. Reload and try again.');
    if(session!==s)return;
    notice('Checking voice access…');
    const access=await fetch('/api/voice/status',{headers:{Authorization:`Bearer ${token}`},cache:'no-store',signal:AbortSignal.timeout(8000)});
    if(!access.ok){
      if(access.status===401)throw new Error('Your session expired or is no longer valid. Sign in again from the dashboard.');
      throw new Error(`Voice access check failed (HTTP ${access.status}). Try again or check the server log.`);
    }
    const capabilities=await access.json();
    if(!capabilities?.can_transmit)throw new Error('Your account needs the voice_transmit permission. Stream viewers can listen through the audience program.');
    radioLink(capabilities.radio);
    if(session!==s)return;
    notice('Waiting for microphone permission…');
    const microphone=navigator.mediaDevices.getUserMedia({audio:{deviceId:$('device').value?{exact:$('device').value}:undefined,channelCount:1,echoCancellation:true,noiseSuppression:noiseSupported?$('noise-suppression').checked:undefined,autoGainControl:false}});
    microphone.then(stream=>{if(session!==s)stream.getTracks().forEach(track=>track.stop());},()=>{});
    const stream=await bounded(microphone,30000,'Microphone access is still pending. Check this browser’s microphone permission and join again.');
    if(session!==s){stream.getTracks().forEach(t=>t.stop());return;}
    s.stream=stream;s.stream.getAudioTracks().forEach(t=>{t.enabled=false;t.onended=()=>{if(session===s)leave('Microphone disconnected. Rejoin to choose another device.');};});
    if(noiseSupported){
      const actual=stream.getAudioTracks()[0]?.getSettings?.().noiseSuppression;
      if(typeof actual==='boolean'){$('noise-suppression').checked=actual;$('noise-status').textContent=actual?'Noise suppression on':'Noise suppression off';}
    }
    s.node=new AudioWorkletNode(s.context,'crew-audio',{numberOfInputs:1,numberOfOutputs:1,outputChannelCount:[1]});
    s.source=s.context.createMediaStreamSource(stream);s.source.connect(s.node);s.node.connect(s.context.destination);
    s.node.port.onmessage=({data:m})=>{
      if(session!==s)return;
      if(m.type==='level')$('level').value=m.value;
      if(m.type==='pcm'&&s.transmitting&&s.ws.readyState===WebSocket.OPEN&&s.ws.bufferedAmount<2560)s.ws.send(m.buffer);
    };
    const url=new URL('/api/voice/ws',location.href);url.protocol=location.protocol==='https:'?'wss:':'ws:';
    s.ws=new WebSocket(url);s.ws.binaryType='arraybuffer';
    s.ws.onopen=()=>{if(session===s)s.ws.send(JSON.stringify({type:'auth',token}));};
    s.ws.onmessage=event=>{
      if(session!==s)return;
      if(event.data instanceof ArrayBuffer){
        if(event.data.byteLength!==644)return;
        const id=new DataView(event.data).getUint32(0,true),buffer=event.data.slice(4);
        s.node.port.postMessage({type:'audio',id,buffer},[buffer]);return;
      }
      let m;try{m=JSON.parse(event.data);}catch{return;}
      if(m.type==='error'){leave(m.message);return;}
      if(m.type==='radio_link'){radioLink(m.radio);}
      if(m.type==='radio_state'){$('radio-enable').checked=m.enabled;session.radio=m.enabled;}

      if(m.type==='broadcast_state')$('broadcast-notice').textContent=m.enabled?'Voice chat audio is included in the audience broadcast, with the program delay.':'Voice chat audio is not included in the audience broadcast.';
      if(m.type==='joined'){clearTimeout(s.joinTimeout);s.ready=true;s.id=m.id;joining=false;transmit();notice('Connected to voice chat.');}
      if(m.type==='roster'){
        const next=new Set(m.peers.map(p=>p.id));
        for(const id of s.peers||[])if(!next.has(id))s.node.port.postMessage({type:'remove',id});s.peers=next;
        $('peers').replaceChildren();$('count').textContent=`${m.peers.length} / 12`;
        for(const p of m.peers){const li=document.createElement('li');li.textContent=`${p.name}${p.id===s.id?' (you)':''} · ${p.transmitting?'Mic open':'Listening'}`;li.className=p.transmitting?'talking':'';$('peers').append(li);}
      }
    };
    s.ws.onclose=()=>{if(session===s)leave('Voice disconnected. Your microphone is off; select Join voice to reconnect.');};
    s.ws.onerror=()=>{if(session===s)leave('Cannot connect to voice. Check your Ground Station connection.');};
    s.ping=setInterval(()=>{if(s.ws.readyState===WebSocket.OPEN)s.ws.send(JSON.stringify({type:'ping'}));},10000);
    s.joinTimeout=setTimeout(()=>{if(session===s&&!s.ready)leave('Voice connection timed out. Check the station connection and join again.');},10000);
    notice('Connecting to voice…');await devices();
  }catch(e){if(session===s)leave(`Cannot join voice: ${e.message}`);}
};
$('noise-suppression').onchange=async()=>{
  const enabled=$('noise-suppression').checked;
  const current=session;
  if(!current?.stream)return;
  current.reconfiguring=true;held=false;transmit();
  let replacement;
  try {
    let actual;
    try {
      for(const track of current.stream.getAudioTracks())await track.applyConstraints({noiseSuppression:{exact:enabled}});
      actual=current.stream.getAudioTracks()[0]?.getSettings?.().noiseSuppression;
    } catch { /* Some devices apply processing options only when opened. */ }
    if(session!==current)return;
    if(actual!==enabled){
      replacement=await navigator.mediaDevices.getUserMedia({audio:{deviceId:$('device').value?{exact:$('device').value}:undefined,channelCount:1,echoCancellation:true,noiseSuppression:{exact:enabled},autoGainControl:false}});
      replacement.getAudioTracks().forEach(track=>{track.enabled=false;});
      if(session!==current)return;
      actual=replacement.getAudioTracks()[0]?.getSettings?.().noiseSuppression;
      if(typeof actual==='boolean'&&actual!==enabled)throw Error('The microphone did not apply the requested setting');
      const source=current.context.createMediaStreamSource(replacement);
      source.connect(current.node);
      const old=current.stream;
      current.source.disconnect();current.source=source;current.stream=replacement;replacement=null;
      current.stream.getAudioTracks().forEach(track=>{track.onended=()=>{if(session===current)leave('Microphone disconnected. Rejoin to choose another device.');};});
      old.getTracks().forEach(track=>{track.onended=null;track.stop();});
    }
    $('noise-suppression').checked=enabled;
    $('noise-status').textContent=actual===enabled?(enabled?'Noise suppression on':'Noise suppression off'):'Requested setting applied; device does not report processing state.';
  } catch(error) {
    if(session===current){
      const actual=current.stream.getAudioTracks()[0]?.getSettings?.().noiseSuppression;
      $('noise-suppression').checked=typeof actual==='boolean'?actual:!enabled;
      $('noise-status').textContent=`Cannot change noise suppression: ${error.message}`;
    }
  } finally {
    replacement?.getTracks().forEach(track=>track.stop());
    current.reconfiguring=false;
    if(session===current)transmit();
    updateControls();
  }
};
$('leave').onclick=()=>leave();$('devices').onclick=devices;
$('mute').onclick=()=>{muted=!muted;held=false;transmit();};
$('mode').onchange=()=>{held=false;if($('mode').value!=='ptt')$('radio-enable').checked=false;transmit();};
$('radio-enable').onchange=()=>{if($('radio-enable').checked)$('mode').value='ptt';held=false;transmit();};
for(const name of ['gain','volume'])$(name).oninput=()=>{$(`${name}-label`).textContent=`${$(name).value}%`;transmit();};
$('ptt').onpointerdown=event=>{if($('ptt').disabled)return;event.preventDefault();$('ptt').setPointerCapture(event.pointerId);held=true;transmit();};
const release=()=>{held=false;transmit();};
for(const event of ['pointerup','pointercancel','lostpointercapture'])$('ptt').addEventListener(event,release);
window.addEventListener('keydown',event=>{
  if(event.code!=='Space'||event.repeat||event.target.closest('input,select,textarea,[contenteditable=true]')||event.target.closest('button')&&event.target!==$('ptt'))return;
  if($('ptt').disabled)return;event.preventDefault();held=true;transmit();
});
window.addEventListener('keyup',event=>{if(event.code==='Space'){if(held)event.preventDefault();release();}});
window.addEventListener('blur',release);
document.addEventListener('visibilitychange',()=>{if(document.hidden)release();});
window.addEventListener('pagehide',()=>leave());
updateControls();

// An embedded tool shares the dashboard session; no credentials are persisted.
if (new URLSearchParams(location.search).get('embedded') === '1' && parent !== window) {
  document.body.dataset.embedded='true';
  $('login').style.display = 'none';
  document.querySelector('nav').style.display = 'none';
  notice('Waiting for the dashboard session…');
  let receivedSession = false;
  window.addEventListener('message', event => {
    if (event.source !== parent || event.data?.type !== 'gs26-session' || typeof event.data.token !== 'string') return;
    if (event.data.theme && typeof event.data.theme==='object') {
      for(const name of ['--gs26-app-background','--gs26-app-text','--gs26-panel-background','--gs26-panel-alt-background','--gs26-border','--gs26-text-muted','--gs26-button-background','--gs26-button-border','--gs26-button-text']) {
        const value=event.data.theme[name];
        if(typeof value==='string' && CSS.supports('color',value))document.documentElement.style.setProperty(name,value);
      }
    }
    if (!event.data.visible) release();
    if (!receivedSession || token !== event.data.token) {
      receivedSession = true;
      leave(); token = event.data.token;
      notice(token ? 'Dashboard session connected. Choose Join voice to enable your microphone.' : 'Sign in from the dashboard to join voice chat.');
      updateControls();
    }
  });
}
