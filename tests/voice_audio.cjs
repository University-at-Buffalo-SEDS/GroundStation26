// Run with node tests/voice_audio.cjs. No microphone, network, or receiver required.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
let Processor;
const sandbox = {sampleRate:48000, AudioWorkletProcessor:class {constructor(){this.messages=[];this.port={postMessage:m=>this.messages.push(m)};}},registerProcessor:(_,c)=>{Processor=c;}};
vm.runInNewContext(fs.readFileSync('backend/src/voice-worklet.js','utf8'),sandbox);
const audio=new Processor();
const input=new Float32Array(128).fill(.25), output=new Float32Array(128);
const run=n=>{for(let i=0;i<n;i++)audio.process([[input]],[[output]]);};
run(375);
assert.equal(audio.messages.filter(m=>m.type==='pcm').length,0,'muted must never send audio');
audio.port.onmessage({data:{type:'settings',transmit:true,gain:2,volume:1}});
run(375); // exactly one second of capture at 48 kHz
const packets=audio.messages.filter(m=>m.type==='pcm');
assert.equal(packets.length,50);
assert.equal(packets[0].buffer.byteLength,640);
assert.equal(new DataView(packets[0].buffer).getInt16(0,true),16384);
audio.port.onmessage({data:{type:'settings',transmit:false,gain:1,volume:1}});
run(10);assert.equal(audio.messages.filter(m=>m.type==='pcm').length,50);
for(let i=0;i<2;i++)audio.port.onmessage({data:{type:'audio',id:1,buffer:packets[0].buffer}});
run(1);assert(output.some(x=>x>0),'received voice must reach playback');
audio.port.onmessage({data:{type:'settings',transmit:false,gain:1,volume:0}});
run(1);assert(output.every(x=>x===0),'speaker mute must silence playback');
for(let i=0;i<100;i++)audio.port.onmessage({data:{type:'audio',id:1,buffer:packets[0].buffer}});
assert(audio.peers.get(1).count<=3200,'jitter buffer must be bounded');
audio.port.onmessage({data:{type:'remove',id:1}});assert.equal(audio.peers.size,0);
console.log('PASS: resampling, PCM framing, PTT mute, gain, playback, speaker volume, and bounded jitter buffer');
