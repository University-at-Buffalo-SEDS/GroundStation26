const {test}=require('node:test');const assert=require('node:assert/strict');const vm=require('node:vm');const fs=require('node:fs');const path=require('node:path');
function element(){return {children:[],parentElement:null,dataset:{},appendChild(child){this.insertBefore(child,null)},insertBefore(child,before){const old=child.parentElement;if(old)old.children.splice(old.children.indexOf(child),1);const index=before?this.children.indexOf(before):this.children.length;this.children.splice(index,0,child);child.parentElement=this;},get firstChild(){return this.children[0]||null}}}
test('the same artwork grows without video and returns to the status bar when video resumes',()=>{
 const window={};vm.runInNewContext(fs.readFileSync(path.join(__dirname,'../backend/src/media/program-art.js'),'utf8'),{window});
 const holder=element(),footer=element(),inset=element(),banner=element();footer.appendChild(inset);footer.appendChild(banner);
 window.gsProgramArt.layout(holder,inset,footer,true);assert.equal(inset.parentElement,holder);assert.equal(footer.dataset.rocketMain,'true');
 for(let i=0;i<100;i++)window.gsProgramArt.layout(holder,inset,footer,true);
 assert.equal(holder.children.length,1);window.gsProgramArt.layout(holder,inset,footer,false);assert.equal(footer.firstChild,inset);assert.equal(footer.children[1],banner);assert.equal(holder.children.length,0);assert.equal(holder.dataset.flat,'false');
});
