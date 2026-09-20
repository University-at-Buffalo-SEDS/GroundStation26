import {Vector3} from './three.module.js';

// Reduced-detail preview of the actual GLB geometry for WebKit without WebGL2.
// Deliberately bounded: no textures/shadows, at most 2000 sampled triangles.
export class SoftwareRenderer {
 constructor(){this.domElement=document.createElement('canvas');this.context=this.domElement.getContext('2d');if(!this.context)throw Error('Canvas2D unavailable');}
 setPixelRatio(){}
 setSize(w,h){this.domElement.width=w;this.domElement.height=h;this.domElement.style.width=w+'px';this.domElement.style.height=h+'px';}
 setAnimationLoop(callback){cancelAnimationFrame(this.frame);if(!callback)return;const run=()=>{callback();this.frame=requestAnimationFrame(run);};this.frame=requestAnimationFrame(run);}
 render(scene,camera){
  const ctx=this.context,w=this.domElement.width,h=this.domElement.height;
  ctx.clearRect(0,0,w,h);scene.updateMatrixWorld(true);camera.updateMatrixWorld(true);
  const meshes=[];let triangles=0;
  scene.traverseVisible(node=>{if(node.isMesh&&node.geometry?.attributes.position){meshes.push(node);triangles+=Math.floor((node.geometry.index?.count??node.geometry.attributes.position.count)/3);}});
  const stride=Math.max(1,Math.ceil(triangles/2000));let ordinal=0,drawn=0;
  const points=[new Vector3(),new Vector3(),new Vector3()];
  ctx.strokeStyle='#93c5fd';ctx.lineWidth=.7;ctx.beginPath();
  for(const mesh of meshes){const positions=mesh.geometry.attributes.position,index=mesh.geometry.index,count=Math.floor((index?.count??positions.count)/3);
   for(let triangle=0;triangle<count;triangle++,ordinal++){
    if(ordinal%stride||drawn>=2000)continue;
    for(let j=0;j<3;j++){const i=triangle*3+j;points[j].fromBufferAttribute(positions,index?index.getX(i):i).applyMatrix4(mesh.matrixWorld).project(camera);}
    if(points.some(p=>!Number.isFinite(p.x)||!Number.isFinite(p.y)||p.z< -1||p.z>1))continue;
    drawn++;ctx.moveTo((points[0].x+1)*w/2,(1-points[0].y)*h/2);
    for(let j=1;j<3;j++)ctx.lineTo((points[j].x+1)*w/2,(1-points[j].y)*h/2);ctx.closePath();
   }
  }
  ctx.stroke();
 }
 dispose(){this.setAnimationLoop(null);}
}
