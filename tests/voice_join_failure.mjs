import assert from 'node:assert/strict';
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'playwright');
const browser=await chromium.launch({headless:true,executablePath:process.env.CHROME_BINARY,args:['--use-fake-device-for-media-stream','--use-fake-ui-for-media-stream']});
try {
 const context=await browser.newContext({permissions:['microphone'],viewport:{width:390,height:844}});
 await context.addInitScript(()=>{AudioContext.prototype.resume=()=>new Promise(()=>{});});
 const page=await context.newPage();
 await page.route('**/assets/media-login.js',r=>r.fulfill({contentType:'text/javascript',body:"export async function signIn(){return {token:'synthetic-producer'}}"}));
 await page.goto('http://127.0.0.1:19090/radio');
 await page.fill('#username','producer');await page.fill('#password','fixture');await page.click('#signin');await page.click('#join');
 await page.locator('#status').filter({hasText:'Starting voice audio'}).waitFor();
 assert(await page.locator('#status').isVisible(),'join feedback is visible beside voice controls');
 await page.waitForFunction(()=>document.querySelector('#status').textContent.includes('Audio could not start'),{},{timeout:10000});
 assert(await page.locator('#join').isEnabled(),'a stalled audio context permits retry');
 assert.equal(await page.locator('#mic-state').textContent(),'Microphone off');
 await page.screenshot({path:'/tmp/gs-voice-mobile-feedback.png',fullPage:true});
 console.log('PASS: immediate visible mobile Join feedback, bounded audio startup, retry and microphone cleanup');
}finally{await browser.close();}
