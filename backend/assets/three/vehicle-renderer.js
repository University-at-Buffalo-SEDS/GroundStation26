import * as T from './three.module.js';
import {GLTFLoader} from './GLTFLoader.js';
// Explicit named-node transforms; no dependence on model-viewer's private scene graph.
class VehicleViewer extends HTMLElement {
 static observedAttributes=['src','data-state'];
 connectedCallback(){
  if(this.renderer)return;
  this.style.display='block';this.style.touchAction='none';this.status=document.createElement('div');this.status.style.cssText='position:absolute;top:12px;left:12px;color:#b9cad7;font:12px system-ui;z-index:2';this.append(this.status);
  try{this.renderer=new T.WebGLRenderer({alpha:true,antialias:true});}catch{this.status.textContent='3D unavailable: WebGL2 is required';return;}
  this.renderer.setPixelRatio(Math.min(devicePixelRatio,2));this.renderer.toneMapping=T.ACESFilmicToneMapping;
  this.append(this.renderer.domElement);this.scene=new T.Scene();this.root=new T.Group();this.scene.add(this.root);
  this.scene.add(new T.HemisphereLight(0xffffff,0x34404b,2.8));const light=new T.DirectionalLight(0xffffff,3);light.position.set(6,10,8);this.scene.add(light);
  this.camera=new T.PerspectiveCamera(38,1,.01,10000);this.theta=.6;this.phi=1.2;this.distance=14;this.center=new T.Vector3();
  this.resize=new ResizeObserver(()=>{const w=this.clientWidth,h=this.clientHeight;if(!w||!h)return;this.renderer.setSize(w,h);this.camera.aspect=w/h;this.camera.updateProjectionMatrix();});this.resize.observe(this);
  this.onpointerdown=e=>{this.setPointerCapture(e.pointerId);this.drag=[e.clientX,e.clientY];};this.onpointerup=()=>this.drag=null;
  this.onpointermove=e=>{if(this.drag){this.theta-=(e.clientX-this.drag[0])*.006;this.phi=T.MathUtils.clamp(this.phi+(e.clientY-this.drag[1])*.006,.1,3);this.drag=[e.clientX,e.clientY];}};
  this.onwheel=e=>{e.preventDefault();this.distance=T.MathUtils.clamp(this.distance*Math.exp(e.deltaY*.001),.3,500);};
  this.clock=new T.Clock();this.load();this.renderer.setAnimationLoop(()=>this.frame());
 }
 attributeChangedCallback(name,old,value){if(old===value)return;if(name==='src'&&this.renderer)this.load();if(name==='data-state'){try{this.state=JSON.parse(value);}catch{this.state={};}}}
 disposeModel(){if(this.model){this.root.remove(this.model);this.model.traverse(n=>{n.geometry?.dispose();for(const m of Array.isArray(n.material)?n.material:n.material?[n.material]:[]){for(const v of Object.values(m))if(v?.isTexture)v.dispose();m.dispose();}});this.model=null;}}
 async load(){const src=this.getAttribute('src');if(!src)return;const generation=this.generation=(this.generation||0)+1;this.status.textContent='Loading model…';try{
   const gltf=await new GLTFLoader().loadAsync(src);if(!this.isConnected||generation!==this.generation){gltf.scene.traverse(n=>{n.geometry?.dispose();});return;}
   this.disposeModel();this.model=gltf.scene;this.root.add(this.model);this.base=new Map();this.smooth=new Map();this.model.traverse(n=>this.base.set(n,{position:n.position.clone(),scale:n.scale.clone(),quaternion:n.quaternion.clone(),visible:n.visible}));
   this.mixer=new T.AnimationMixer(this.model);this.clips=gltf.animations;this.activeClip=null;
   const box=new T.Box3().setFromObject(this.model);box.getCenter(this.center);const size=box.getSize(new T.Vector3());this.distance=Math.max(size.length()*1.45,2);this.status.textContent='';this.dataset.loaded='true';
  }catch(e){this.status.textContent='Model could not load. Check the GLB and its assets.';this.dataset.loaded='false';}
 }
 frame(){
  if(!this.model)return;const dt=Math.min(this.clock.getDelta(),.1),state=this.state||{};
  if(state.orbit&&state.orbit!==this.orbit){const parts=state.orbit.trim().split(/\s+/);const theta=parseFloat(parts[0]),phi=parseFloat(parts[1]),radius=parseFloat(parts[2]);if(Number.isFinite(theta))this.theta=T.MathUtils.degToRad(theta);if(Number.isFinite(phi))this.phi=T.MathUtils.clamp(T.MathUtils.degToRad(phi),.1,3);if(Number.isFinite(radius)&&radius>0)this.distance=radius;this.orbit=state.orbit;}
  if(state.clip!==this.activeClip){this.mixer.stopAllAction();const clip=this.clips.find(c=>c.name===state.clip);if(clip)this.mixer.clipAction(clip).play();this.activeClip=state.clip;}
  for(const node of this.driven||[]){const b=this.base.get(node);if(b){node.position.copy(b.position);node.quaternion.copy(b.quaternion);node.scale.copy(b.scale);node.visible=b.visible;}}
  this.mixer.update(dt);
  const modified=new Set();let missing=0,unknown=0;
  for(const motion of state.motions||[]){const node=this.model.getObjectByName(motion.node);if(!node){missing++;continue;}const base=this.base.get(node);if(!base)continue;
   if(!modified.has(node)){node.position.copy(base.position);node.quaternion.copy(base.quaternion);node.scale.copy(base.scale);node.visible=base.visible;modified.add(node);}
   if(motion.value==null||!Number.isFinite(motion.value)){unknown++;continue;}
   const target=motion.from+(motion.to-motion.from)*T.MathUtils.clamp(motion.value,0,1),key=motion.node+':'+motion.transform+':'+motion.axis.join(',');
   const prior=this.smooth.get(key)??target,v=T.MathUtils.lerp(prior,target,1-Math.exp(-dt*10));this.smooth.set(key,v);
   const axis=new T.Vector3(...motion.axis);
   if(motion.transform==='rotate'&&axis.lengthSq()>0)node.quaternion.multiply(new T.Quaternion().setFromAxisAngle(axis.normalize(),T.MathUtils.degToRad(v)));
   if(motion.transform==='translate')node.position.addScaledVector(axis,v);
   if(motion.transform==='scale')node.scale.multiply(new T.Vector3(1+axis.x*(v-1),1+axis.y*(v-1),1+axis.z*(v-1)));
   if(motion.transform==='visible')node.visible=v>=.5;
  }
  this.driven=modified;
  this.status.textContent=missing?missing+' mapped model node(s) missing':unknown?unknown+' component(s): no telemetry':'';
  const a=state.attitude||[0,0,0];this.root.rotation.set(...a.map(v=>T.MathUtils.degToRad(v||0)));
  this.camera.position.set(this.center.x+this.distance*Math.sin(this.phi)*Math.sin(this.theta),this.center.y+this.distance*Math.cos(this.phi),this.center.z+this.distance*Math.sin(this.phi)*Math.cos(this.theta));this.camera.lookAt(this.center);this.renderer.render(this.scene,this.camera);
 }
 disconnectedCallback(){this.generation++;this.resize?.disconnect();this.renderer?.setAnimationLoop(null);this.disposeModel();this.renderer?.dispose();this.renderer?.domElement.remove();this.status?.remove();this.renderer=null;}
}
if(!customElements.get('gs-vehicle-viewer'))customElements.define('gs-vehicle-viewer',VehicleViewer);
