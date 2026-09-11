// Procedural, unbranded GLB assets. Run: node scripts/build_gse_models.mjs
// Dimensions are illustrative; these are operator visuals, not CAD drawings.
import fs from 'node:fs';
import path from 'node:path';
const output = path.resolve('backend/assets/models');
fs.mkdirSync(output,{recursive:true});
function scene(){
  const doc={asset:{version:'2.0',generator:'GroundStation26 minimal site models'},scene:0,scenes:[{nodes:[]}],nodes:[],meshes:[],materials:[],accessors:[],bufferViews:[],buffers:[],animations:[]};
  const chunks=[];let length=0;
  const material=(name,color,metal=.1,rough=.55)=>{doc.materials.push({name,pbrMetallicRoughness:{baseColorFactor:[...color,1],metallicFactor:metal,roughnessFactor:rough}});return doc.materials.length-1;};
  const white=material('ceramic',[.88,.9,.91]),black=material('graphite',[.055,.065,.075]),steel=material('titanium',[.3,.35,.4],.75,.3),blue=material('nitrogen',[.12,.58,.9]),green=material('nitrous',[.28,.85,.67]);
  const accessor=(data,size)=>{const values=new Float32Array(data);const bytes=Buffer.from(values.buffer);const view=doc.bufferViews.length;doc.bufferViews.push({buffer:0,byteOffset:length,byteLength:bytes.length});chunks.push(bytes);length+=bytes.length;const count=data.length/size;const item={bufferView:view,componentType:5126,count,type:{1:'SCALAR',3:'VEC3',4:'VEC4'}[size]};if(count){item.min=Array.from({length:size},(_,i)=>Math.min(...data.filter((_,j)=>j%size===i)));item.max=Array.from({length:size},(_,i)=>Math.max(...data.filter((_,j)=>j%size===i)));}doc.accessors.push(item);return doc.accessors.length-1;};
  const mesh=(name,vertices,normals,mat)=>{doc.meshes.push({name,primitives:[{attributes:{POSITION:accessor(vertices,3),NORMAL:accessor(normals,3)},material:mat}]});return doc.meshes.length-1;};
  const boxMesh=mat=>{const p=[],n=[];const corners=[[-.5,-.5,-.5],[.5,-.5,-.5],[.5,.5,-.5],[-.5,.5,-.5],[-.5,-.5,.5],[.5,-.5,.5],[.5,.5,.5],[-.5,.5,.5]];for(const [face,normal] of [[[4,5,6,7],[0,0,1]],[[1,0,3,2],[0,0,-1]],[[5,1,2,6],[1,0,0]],[[0,4,7,3],[-1,0,0]],[[7,6,2,3],[0,1,0]],[[0,1,5,4],[0,-1,0]]])for(const i of [0,1,2,0,2,3]){p.push(...corners[face[i]]);n.push(...normal);}return mesh('box',p,n,mat);};
  const cylinderMesh=(mat,top=1)=>{const p=[],n=[],segments=48;const add=(v,no)=>{p.push(...v);n.push(...no);};for(let i=0;i<segments;i++){const a=i/segments*Math.PI*2,b=(i+1)/segments*Math.PI*2;const va=[Math.cos(a),-.5,Math.sin(a)],vb=[Math.cos(b),-.5,Math.sin(b)],ta=[top*Math.cos(a),.5,top*Math.sin(a)],tb=[top*Math.cos(b),.5,top*Math.sin(b)];const normal=t=>{const v=[Math.cos(t),1-top,Math.sin(t)],l=Math.hypot(...v);return v.map(x=>x/l);};for(const [v,t]of [[va,a],[ta,a],[tb,b],[va,a],[tb,b],[vb,b]])add(v,normal(t));for(const v of [[0,-.5,0],va,vb])add(v,[0,-1,0]);for(const v of [[0,.5,0],tb,ta])add(v,[0,1,0]);}return mesh('cylinder',p,n,mat);};
  const cache=new Map();const shape=(kind,mat)=>{const key=kind+mat;if(!cache.has(key))cache.set(key,kind==='box'?boxMesh(mat):cylinderMesh(mat,kind==='cone'?0:1));return cache.get(key);};
  const node=(name,kind,mat,translation,scale,rotation)=>{const index=doc.nodes.length;doc.nodes.push({name,mesh:shape(kind,mat),translation,scale,...(rotation?{rotation}:{})});doc.scenes[0].nodes.push(index);return index;};
  const box=(name,mat,at,size)=>node(name,'box',mat,at,size);
  const cylinder=(name,mat,at,radius,height)=>node(name,'cylinder',mat,at,[radius,height,radius]);
  const pipe=(name,mat,a,b,r=.045)=>{const d=b.map((x,i)=>x-a[i]),l=Math.hypot(...d),u=d.map(x=>x/l),dot=u[1];let q=dot < -.99999?[1,0,0,0]:[u[2],0,-u[0],1+dot];const ql=Math.hypot(...q);q=q.map(x=>x/ql);return node(name,'cylinder',mat,a.map((x,i)=>(x+b[i])/2),[r,l,r],q);};
  const rocket=(x=0,z=0)=>{cylinder('booster',white,[x,2.25,z],.28,3.6);cylinder('interstage',black,[x,4.12,z],.28,.18);cylinder('sustainer',white,[x,5.35,z],.28,2.25);node('nose','cone',white,[x,7,z],[.28,1.05,.28]);cylinder('engine',steel,[x,.33,z],.2,.25);for(const angle of [0,Math.PI/2,Math.PI,Math.PI*1.5]){const dx=Math.cos(angle),dz=Math.sin(angle);box('fin',black,[x+dx*.38,.9,z+dz*.38],[Math.abs(dx)*.38+.045,.7,Math.abs(dz)*.38+.045]);}};
  const animate=(name,nodeId,points)=>{const input=accessor([0,1,2,3],1),out=accessor(points.flat(),3);doc.animations.push({name,samplers:[{input,output:out,interpolation:'LINEAR'}],channels:[{sampler:0,target:{node:nodeId,path:'translation'}}]});};
  const save=name=>{let binary=Buffer.concat(chunks);doc.buffers=[{byteLength:binary.length}];let json=Buffer.from(JSON.stringify(doc));json=Buffer.concat([json,Buffer.alloc((4-json.length%4)%4,32)]);binary=Buffer.concat([binary,Buffer.alloc((4-binary.length%4)%4)]);const header=Buffer.alloc(12);header.write('glTF');header.writeUInt32LE(2,4);header.writeUInt32LE(12+8+json.length+8+binary.length,8);const chunk=(bytes,type)=>{const h=Buffer.alloc(8);h.writeUInt32LE(bytes.length);h.write(type,4);return Buffer.concat([h,bytes]);};fs.writeFileSync(path.join(output,name),Buffer.concat([header,chunk(json,'JSON'),chunk(binary,'BIN\0')]));console.log(`${name}: ${doc.nodes.length} nodes, ${doc.animations.length} clips`);};
  return {doc,material,white,black,steel,blue,green,box,cylinder,pipe,rocket,animate,save};
}
const site=scene();
site.box('pad',site.black,[0,-.1,0],[10,.2,7]);
site.rocket(2,0);
for(const x of [2.9,3.65])for(const z of [-.8,.1])site.box('tower upright',site.steel,[x,3.9,z],[.1,8,.1]);
for(let y=.5;y+.8<=7.9;y+=.8){site.box('tower rail',site.steel,[3.28,y,-.8],[.85,.06,.06]);site.pipe('tower brace',site.steel,[2.9,y,-.8],[3.65,y+.8,-.8],.025);}
site.box('umbilical boom',site.white,[2.7,5.2,-.3],[1.8,.12,.12]);
for(const [name,x,color,radius,height]of [['nitrogen vessel',-3,site.blue,.48,2.7],['nitrous vessel',-1.4,site.green,.7,3.5]]){
site.cylinder(name,site.white,[x,height/2+.2,-1],radius,height);site.cylinder(name+' band',color,[x,1.2,-1],radius+.008,.12);site.box(name+' foot',site.steel,[x,.1,-1],[radius*2.3,.2,radius*2.3]);}
site.box('fill manifold',site.white,[-.8,.6,1.5],[2.9,1.2,.55]);
for(const [i,name]of ['pilot','vent','dump','nitrogen','nitrous'].entries()){const mat=site.material('valve-'+name,[.22,.27,.31]);site.cylinder('valve '+name,mat,[-1.9+i*.55,.72,1.85],.13,.16);}
site.pipe('nitrogen supply',site.blue,[-3,.35,-1],[-3,.35,1.5]);site.pipe('nitrogen header',site.blue,[-3,.35,1.5],[-1,.35,1.5]);
site.pipe('nitrous supply',site.green,[-1.4,.45,-1],[-1.4,.45,.95]);site.pipe('fill line',site.steel,[-.8,.4,1.5],[2,.4,1.5]);site.pipe('umbilical line',site.steel,[2,.4,1.5],[2,1.1,0]);
site.pipe('vent riser',site.steel,[3.8,.2,2.2],[3.8,3,2.2]);site.box('control pedestal',site.steel,[-3.7,.7,2.1],[.65,1.4,.4]);site.box('control display',site.black,[-3.7,1.28,2.33],[.5,.32,.03]);
const marker=site.cylinder('sequence activity',site.blue,[-3,.47,-1],.075,.11);
site.animate('nitrogen-test',marker,[[-3,.47,-1],[-3,.47,1.5],[-1,.47,1.5],[-3,.47,-1]]);
site.animate('nitrous-fill',marker,[[-1.4,.55,-1],[-1.4,.55,.95],[2,.55,1.5],[-1.4,.55,-1]]);
site.animate('dumping',marker,[[2,.5,1.5],[3.8,.5,2.2],[3.8,2.9,2.2],[2,.5,1.5]]);
site.save('gse-site.glb');
const rocket=scene();rocket.rocket();rocket.save('vehicle.glb');
