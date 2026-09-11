const fs=require('node:fs'),assert=require('node:assert/strict');
function glb(file){const b=fs.readFileSync(file);assert.equal(b.toString('utf8',0,4),'glTF');return JSON.parse(b.toString('utf8',20,20+b.readUInt32LE(12)));}
const model=glb('backend/assets/models/vehicle.glb');
assert.equal(model.nodes.filter(n=>n.name==='stage-1').length,1);
assert.equal(model.nodes.filter(n=>n.name==='fin').length,4);
assert.equal(model.nodes.filter(n=>/airbrake|booster-stage|sustainer-stage/.test(n.name)).length,0);
for(const n of model.nodes.filter(n=>n.name.startsWith('fin-pivot-')))assert.ok(n.translation[1]<1.5,'Fin must remain aft');
const site=glb('backend/assets/models/gse-site.glb');
assert.ok(site.nodes.some(n=>n.name==='airframe-upper'));
assert.ok(!site.nodes.some(n=>n.name==='interstage'||n.name==='sustainer'));
console.log('PASS: single-stage rocket, four aft fins, no upper fins/panels, matching GSE airframe');
