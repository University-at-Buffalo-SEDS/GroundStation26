// Wire audio: 320 signed PCM16 samples at 16 kHz per packet (20 ms).
class CrewAudio extends AudioWorkletProcessor {
  constructor() {
    super(); this.transmit=false; this.gain=1; this.volume=1; this.peers=new Map();
    this.samples=new Int16Array(320); this.used=0; this.phase=0; this.sum=0; this.n=0; this.meter=0;
    this.port.onmessage=({data:m})=>{
      if(m.type==='settings') { this.transmit=m.transmit; this.gain=m.gain; this.volume=m.volume; if(!this.transmit){this.used=0;this.phase=0;this.sum=0;this.n=0;} }
      if(m.type==='remove') this.peers.delete(m.id);
      if(m.type==='reset') this.peers.clear();
      if(m.type==='audio') {
        let p=this.peers.get(m.id); if(!p){p={buffer:new Float32Array(4096),read:0,write:0,count:0,phase:0,playing:false,idle:0};this.peers.set(m.id,p);}p.idle=0;
        const view=new DataView(m.buffer);
        if(p.count+view.byteLength/2>3200){p.read=p.write;p.count=0;p.phase=0;p.playing=false;}
        for(let i=0;i<view.byteLength;i+=2){p.buffer[p.write]=view.getInt16(i,true)/32768;p.write=(p.write+1)%4096;p.count++;}
      }
    };
  }
  process(inputs, outputs) {
    const input=inputs[0]?.[0], output=outputs[0][0]; let peak=0;
    for(let i=0;i<output.length;i++) {
      const mic=Math.max(-1,Math.min(1,(input?.[i]||0)*this.gain));peak=Math.max(peak,Math.abs(mic));
      if(this.transmit) {
        this.sum+=mic;this.n++;this.phase+=16000;
        if(this.phase>=sampleRate) {
          this.phase-=sampleRate;const sample=this.sum/this.n;this.sum=0;this.n=0;
          this.samples[this.used++]=Math.round(sample*(sample<0?32768:32767));
          if(this.used===320){const buffer=this.samples.buffer;this.port.postMessage({type:'pcm',buffer},[buffer]);this.samples=new Int16Array(320);this.used=0;}
        }
      }
      let mixed=0;
      for(const p of this.peers.values()) {
        if(!p.playing&&p.count>=640)p.playing=true; // 40 ms jitter prebuffer
        if(!p.playing)continue;
        if(p.count<2){p.count=0;p.read=p.write;p.phase=0;p.playing=false;continue;}
        mixed+=p.buffer[p.read]*(1-p.phase)+p.buffer[(p.read+1)%4096]*p.phase;
        p.phase+=16000/sampleRate;
        while(p.phase>=1&&p.count){p.phase--;p.read=(p.read+1)%4096;p.count--;}
      }
      output[i]=Math.max(-1,Math.min(1,mixed*this.volume));
    }
    for(const [id,p] of this.peers){if(!p.playing&&p.count<2){p.idle+=output.length;if(p.idle>sampleRate*5)this.peers.delete(id);}}
    this.meter+=output.length;
    if(this.meter>=sampleRate/10){this.port.postMessage({type:'level',value:peak});this.meter=0;}
    return true;
  }
}
registerProcessor('crew-audio',CrewAudio);
