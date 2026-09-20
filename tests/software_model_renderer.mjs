import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import * as T from '../backend/assets/three/three.module.js';
import {GLTFLoader} from '../backend/assets/three/GLTFLoader.js';
import {SoftwareRenderer} from '../backend/assets/three/software-renderer.js';

for(const name of ['vehicle','gse-site']) {
 test(`Canvas2D compatibility renders actual ${name} GLB without WebGL`,async()=>{
  let triangles=0;
  const context={clearRect(){},beginPath(){},moveTo(x,y){assert.ok(Number.isFinite(x)&&Number.isFinite(y));},lineTo(x,y){assert.ok(Number.isFinite(x)&&Number.isFinite(y));},closePath(){triangles++;},stroke(){}};
  globalThis.document={createElement(tag){assert.equal(tag,'canvas');return {style:{},getContext(kind){assert.equal(kind,'2d');return context;}};}};
  const bytes=await fs.readFile(new URL(`../backend/assets/models/${name}.glb`,import.meta.url));
  const gltf=await new GLTFLoader().parseAsync(bytes.buffer.slice(bytes.byteOffset,bytes.byteOffset+bytes.byteLength),'');
  const scene=new T.Scene();scene.add(gltf.scene);
  const box=new T.Box3().setFromObject(gltf.scene),center=box.getCenter(new T.Vector3()),size=box.getSize(new T.Vector3());
  const camera=new T.PerspectiveCamera(38,800/600,.01,10000);
  camera.position.copy(center).add(new T.Vector3(size.length(),size.length(),size.length()));camera.lookAt(center);
  const renderer=new SoftwareRenderer();renderer.setSize(800,600);renderer.render(scene,camera);
  assert.ok(triangles>0,'must render model geometry, not a placeholder');
  assert.ok(triangles<=2000,'Pi preview work must be bounded');
 });
}
