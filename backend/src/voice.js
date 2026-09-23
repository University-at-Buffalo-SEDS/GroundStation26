import {signIn} from '/assets/media-login.js';
const $=id=>document.getElementById(id);
let token='', session=null, muted=false, held=false, joining=false;
const notice=text=>{$('status').textContent=text;};
function updateControls(){
  $('join').disabled=!token||!!session||joining;$('leave').disabled=!session;
  $('device').disabled=!!session||joining;$('mode').disabled=joining;
  $('mute').disabled=!session?.ready;$('ptt').disabled=!session?.ready||muted||$('mode').value!=='ptt';
  $('logout').disabled=!token;$('signin').disabled=!!session||joining;
}
function transmit(){
  const enabled=!!session?.ready&&!muted&&($('mode').value==='open'||held);
  if(session){
    if(session.transmitting!==enabled){
      session.transmitting=enabled;
      if(session.ws?.readyState===WebSocket.OPEN)session.ws.send(JSON.stringify({type:'transmitting',enabled}));
    }
    session.stream?.getAudioTracks().forEach(t=>{t.enabled=enabled;});
    session.node?.port.postMessage({type:'settings',transmit:enabled,gain:Number($('gain').value)/100,volume:Number($('volume').value)/100});
  }
  $('ptt').setAttribute('aria-pressed',String(enabled&&$('mode').value==='ptt'));
  $('mute').setAttribute('aria-pressed',String(muted));$('mute').textContent=muted?'Unmute microphone':'Mute microphone';
  $('mic-state').textContent=!session?'Microphone off':enabled?'Transmitting to crew':muted?'Microphone muted':'Listening · hold to talk';
  updateControls();
}
function leave(message='Left voice. Microphone released.'){
  const old=session;session=null;held=false;joining=false;
  if(old){clearInterval(old.ping);old.ws?.close();old.stream?.getTracks().forEach(t=>t.stop());old.node?.disconnect();old.context?.close().catch(()=>{});}
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
  joining=true;muted=false;held=false;updateControls();
  const s={ready:false,transmitting:false};session=s;
  try{
    s.context=new AudioContext();await s.context.resume();
    await s.context.audioWorklet.addModule('/assets/voice-worklet.js');
    if(session!==s)return;
    const access=await fetch('/api/voice/status',{headers:{Authorization:`Bearer ${token}`}});
    if(!access.ok || !(await access.json()).can_transmit)throw new Error('Your account needs the voice_transmit permission. Stream viewers can listen through the audience program.');
    if(session!==s)return;
    const stream=await navigator.mediaDevices.getUserMedia({audio:{deviceId:$('device').value?{exact:$('device').value}:undefined,channelCount:1,echoCancellation:true,noiseSuppression:true,autoGainControl:false}});
    if(session!==s){stream.getTracks().forEach(t=>t.stop());return;}
    s.stream=stream;s.stream.getAudioTracks().forEach(t=>{t.enabled=false;t.onended=()=>{if(session===s)leave('Microphone disconnected. Rejoin to choose another device.');};});
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
      if(m.type==='broadcast_state')$('broadcast-notice').textContent=m.enabled?'Crew audio is included in the audience broadcast, with the program delay.':'Crew audio is not included in the audience broadcast.';
      if(m.type==='joined'){s.ready=true;s.id=m.id;joining=false;transmit();notice('Connected to crew voice.');}
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
    notice('Connecting to voice…');await devices();
  }catch(e){if(session===s)leave(`Cannot join voice: ${e.message}`);}
};
$('leave').onclick=()=>leave();$('devices').onclick=devices;
$('mute').onclick=()=>{muted=!muted;held=false;transmit();};
$('mode').onchange=()=>{held=false;transmit();};
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
  $('login').style.display = 'none';
  document.querySelector('nav').style.display = 'none';
  notice('Waiting for the dashboard session…');
  let receivedSession = false;
  window.addEventListener('message', event => {
    if (event.source !== parent || event.data?.type !== 'gs26-session' || typeof event.data.token !== 'string') return;
    if (!event.data.visible) release();
    if (!receivedSession || token !== event.data.token) {
      receivedSession = true;
      leave(); token = event.data.token;
      notice(token ? 'Dashboard session connected. Choose Join voice to enable your microphone.' : 'Sign in from the dashboard to join crew voice.');
      updateControls();
    }
  });
}
