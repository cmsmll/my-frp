// Run after `cargo build`: NODE_PATH=<directory containing playwright> node tests/ui-smoke.cjs
const { chromium } = require('playwright');
const { spawn } = require('node:child_process');
const { once } = require('node:events');
const fs = require('node:fs/promises');
const path = require('node:path');
const os = require('node:os');
const http = require('node:http');
const net = require('node:net');
const assert = require('node:assert/strict');
const password = 'browser-integration-password';
const children = [];
const errors = [];
let browser, service, dir;
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
async function port() { const s = net.createServer(); s.listen(0,'127.0.0.1'); await once(s,'listening'); const p=s.address().port; await new Promise(r=>s.close(r)); return p; }
function start(name,args,initialPassword=password) { const exe=path.resolve(process.env.MYFRP_BIN_DIR || 'target/debug',name+(process.platform==='win32'?'.exe':'')); const env={...process.env,RUST_LOG:'my_frp=info'};if(initialPassword===null)delete env.MYFRP_ADMIN_PASSWORD;else env.MYFRP_ADMIN_PASSWORD=initialPassword;const child=spawn(exe,args,{windowsHide:true,stdio:['ignore','ignore','pipe'],env}); child.stderr.on('data',d=>process.stderr.write(d)); children.push(child); }
async function ready(url) { for(let i=0;i<100;i++){try{if((await fetch(url+'/api/meta')).ok)return;}catch{}await delay(100);}throw new Error('Startup timeout: '+url); }
async function login(page,url,chosenPassword=password,forced=false) { await page.bringToFront();page.on('pageerror',e=>errors.push(e.message)); await page.goto(url); await page.locator('#password').fill(chosenPassword); await page.locator('#login-form button').click(); await page.locator(forced?'#password-dialog':'#dashboard').waitFor({state:'visible'}); }
async function changePassword(page,current,next,confirmation=next) { await page.locator('#current-password').fill(current);await page.locator('#new-password').fill(next);await page.locator('#confirm-password').fill(confirmation);await page.locator('#password-form button[type=submit]').click(); }
async function checkLogs(page,secret) {
  await page.bringToFront();
  const requestsBefore=page.logRequests;
  const initial=page.waitForResponse(r=>new URL(r.url()).pathname==='/api/logs'&&r.request().method()==='GET');
  await page.locator('#nav-logs').click();const result=await (await initial).json();assert.ok(result.items.length>0,'runtime logs missing');
  assert.equal(page.logRequests,requestsBefore+1,'entering logs made duplicate requests');
  await page.locator('#logs-section').waitFor({state:'visible'});await page.locator('#log-auto-refresh').uncheck();
  const filteredResponse=page.waitForResponse(r=>{const u=new URL(r.url());return u.pathname==='/api/logs'&&u.searchParams.get('level')==='INFO';});
  await page.locator('#log-level').selectOption('INFO');await page.locator('#logs-filter button[type=submit]').click();
  const filtered=await (await filteredResponse).json();assert.ok(filtered.items.length>0);assert.ok(filtered.items.every(r=>r.level==='INFO'));
  const message=filtered.items[0].message;
  await page.waitForFunction(text=>document.querySelector('#logs-list').textContent.includes(text),message);
  const downloaded=page.waitForEvent('download');await page.locator('#log-export').click();const download=await downloaded;const text=await fs.readFile(await download.path(),'utf8');
  assert.ok(text.includes(message),'filtered export omitted displayed logs');assert.ok(!text.includes(secret),'export exposed JWT');
  const emptyResponse=page.waitForResponse(r=>{const u=new URL(r.url());return u.pathname==='/api/logs'&&u.searchParams.get('q')==='unmatched-browser-log-query-17592316';});
  await page.locator('#log-query').fill('unmatched-browser-log-query-17592316');await page.locator('#logs-filter button[type=submit]').click();
  assert.deepEqual((await (await emptyResponse).json()).items,[]);
  await page.waitForFunction(()=>document.querySelector('#logs-list').textContent.includes('暂无'));
  const resetResponse=page.waitForResponse(r=>{const u=new URL(r.url());return u.pathname==='/api/logs'&&!u.searchParams.get('level')&&!u.searchParams.get('q');});
  await page.locator('#log-query').fill('');await page.locator('#log-level').selectOption('');await page.locator('#logs-filter button[type=submit]').click();await resetResponse;
}
async function checkLogLifecycle(page) {
  await page.bringToFront();
  let release;
  const gate=new Promise(resolve=>{release=resolve;});
  const isLogs=url=>url.pathname==='/api/logs';
  const handler=async route=>{await gate;await route.continue().catch(()=>{});};
  await page.route(isLogs,handler);
  try {
    const pending=page.waitForRequest(request=>isLogs(new URL(request.url())));
    const failed=page.waitForEvent('requestfailed',{predicate:request=>isLogs(new URL(request.url())),timeout:10000});
    await page.locator('#nav-logs').click();await pending;await page.locator('#nav-tunnels').click();release();
    const request=await failed;assert.match(request.failure().errorText,/ABORTED|CANCELLED/i,'leaving logs did not abort its in-flight request');
    assert.equal(await page.locator('#logs-error').textContent(),'','cancelled request surfaced as a log error');
  } finally {release();await page.unroute(isLogs,handler);}
  const loaded=page.waitForResponse(response=>isLogs(new URL(response.url())));
  await page.locator('#nav-logs').click();await loaded;
  const enabled=page.waitForResponse(response=>isLogs(new URL(response.url())));
  await page.locator('#log-auto-refresh').check();await enabled;
  await page.evaluate(()=>{Object.defineProperty(document,'hidden',{configurable:true,value:true});document.dispatchEvent(new Event('visibilitychange'));});
  const hiddenRequests=page.logRequests;await delay(5500);assert.equal(page.logRequests,hiddenRequests,'logs polled while document was hidden');
  const resumed=page.waitForResponse(response=>isLogs(new URL(response.url())));
  await page.evaluate(()=>{delete document.hidden;document.dispatchEvent(new Event('visibilitychange'));});await resumed;
  await page.locator('#nav-tunnels').click();
  console.log('UI progress: in-flight log cancellation and visibility pause/resume passed.');
}
async function main() {
  dir=await fs.mkdtemp(path.join(os.tmpdir(),'my-frp-ui-'));
  const [sa,ta,ca,defaultAdminPort,publicPort]=await Promise.all([port(),port(),port(),port(),port()]);
  service=http.createServer((req,res)=>{res.setHeader('Connection','close');res.end('my-frp browser roundtrip');}); service.listen(0,'127.0.0.1');await once(service,'listening');
  const serverConfig=path.join(dir,'server-config.json');
  await fs.writeFile(serverConfig,JSON.stringify({data_dir:'server',admin_listen:`127.0.0.1:${sa}`,tunnel_listen:`127.0.0.1:${ta}`,public_url:`ws://127.0.0.1:${ta}`,admin_password:password,allow_insecure:true,port_min:1024,port_max:65535},null,2));
  start('myfrps',[serverConfig]);
  start('myfrpc',['--data-dir',path.join(dir,'client'),'--admin-listen',`127.0.0.1:${ca}`,'--allow-insecure']);
  start('myfrpc',['--data-dir',path.join(dir,'default-client'),'--admin-listen',`127.0.0.1:${defaultAdminPort}`,'--allow-insecure'],null);
  await Promise.all([ready(`http://127.0.0.1:${sa}`),ready(`http://127.0.0.1:${ca}`),ready(`http://127.0.0.1:${defaultAdminPort}`)]);
  browser=await chromium.launch({channel:'msedge',headless:true});
  const context=await browser.newContext({viewport:{width:1512,height:982}});const server=await context.newPage(),client=await context.newPage(),fresh=await context.newPage();
  for(const page of [server,client,fresh]){page.logRequests=0;page.on('request',request=>{if(['/api/logs','/api/logs/export'].includes(new URL(request.url()).pathname))page.logRequests++;});}
  const defaultUrl=`http://127.0.0.1:${defaultAdminPort}`,newPassword='new-browser-integration-password';
  await login(fresh,defaultUrl,'admin',true);assert.ok(await fresh.locator('#dashboard').isHidden(),'default password exposed dashboard');
  await fresh.reload();await fresh.locator('#password-dialog').waitFor({state:'visible'});await fresh.keyboard.press('Escape');assert.ok(await fresh.locator('#password-dialog').isVisible(),'forced change dismissed with Escape');
  await changePassword(fresh,'wrong-current-password',newPassword);await fresh.waitForFunction(()=>document.querySelector('#password-error').textContent.length>0);
  await changePassword(fresh,'admin',newPassword,'mismatched-browser-password');await fresh.waitForFunction(()=>document.querySelector('#password-error').textContent.includes('不一致'));
  await changePassword(fresh,'admin',newPassword);await fresh.locator('#login-panel').waitFor({state:'visible'});await login(fresh,defaultUrl,newPassword);assert.ok(await fresh.locator('#password-dialog').isHidden());
  console.log('UI progress: default-admin required change, validation and relogin passed.');
  await login(server,`http://127.0.0.1:${sa}`);await server.locator('#add').click();await server.locator('#name').fill('开发环境 Web');await server.locator('#port').fill(String(publicPort));await server.locator('#edit-form button[type=submit]').click();
  await server.locator('#token-dialog').waitFor({state:'visible'});let token=await server.locator('#issued-token').inputValue();assert.ok(token.length>100);await server.locator('#close-token').click();
  await login(client,`http://127.0.0.1:${ca}`);await client.locator('#add').click();await client.locator('#name').fill('本机应用');await client.locator('#local-addr').fill(`127.0.0.1:${service.address().port}`);await client.locator('#token-input').fill(token);await client.locator('#edit-form button[type=submit]').click();
  await client.locator('#editor').waitFor({state:'hidden'});await client.locator('#rows .badge').filter({hasText:'在线'}).waitFor();assert.equal(await (await fetch(`http://127.0.0.1:${publicPort}`)).text(),'my-frp browser roundtrip');
  await server.locator('#rows .badge').filter({hasText:'在线'}).waitFor();
  console.log('UI progress: real server/client tunnel creation and forwarding passed.');
  const out=path.resolve('target/ui-preview');await fs.mkdir(out,{recursive:true});await server.screenshot({path:path.join(out,'server.png'),fullPage:true});await client.screenshot({path:path.join(out,'client.png'),fullPage:true});
  await server.bringToFront();await server.locator('[data-action=rotate]').click();await server.locator('#confirm-form button[type=submit]').click();await server.locator('#token-dialog').waitFor({state:'visible'});token=await server.locator('#issued-token').inputValue();await server.locator('#close-token').click();
  await client.bringToFront();await client.locator('[data-action=edit]').click();await client.locator('#token-input').fill(token);await client.locator('#edit-form button[type=submit]').click();await client.locator('#editor').waitFor({state:'hidden'});await client.locator('#rows .badge').filter({hasText:'在线'}).waitFor();
  assert.equal(await (await fetch(`http://127.0.0.1:${publicPort}`)).text(),'my-frp browser roundtrip');
  await client.setViewportSize({width:390,height:844});await client.screenshot({path:path.join(out,'client-mobile.png'),fullPage:true});
  assert.ok(await client.evaluate(()=>document.documentElement.scrollWidth<=window.innerWidth),'mobile page overflow');
  await server.bringToFront();await server.locator('#nav-audit').click();await server.locator('.audit-row').first().waitFor();
  await client.bringToFront();await client.locator('[data-action=delete]').click();await client.locator('#confirm-form button[type=submit]').click();await client.locator('#empty').waitFor({state:'visible'});
  for(const page of [server,client,fresh])assert.equal(page.logRequests,0,'logs fetched before entering runtime logs');
  console.log('UI progress: rotation, audit, deletion and zero pre-entry log requests passed.');
  await checkLogs(server,token);await server.screenshot({path:path.join(out,'server-logs.png'),fullPage:true});
  await checkLogs(client,token);await client.screenshot({path:path.join(out,'client-logs-mobile.png'),fullPage:true});assert.ok(await client.evaluate(()=>document.documentElement.scrollWidth<=window.innerWidth),'mobile logs page overflow');
  const inactiveRequests=[server.logRequests,client.logRequests];
  for(const page of [server,client]){await page.bringToFront();await page.locator('#nav-tunnels').click();await page.evaluate(()=>{const select=document.querySelector('#log-level');select.value='WARN';select.dispatchEvent(new Event('change',{bubbles:true}));});}
  await delay(5500);
  assert.deepEqual([server.logRequests,client.logRequests],inactiveRequests,'logs kept polling or fetched inactive filters after leaving logs');
  console.log('UI progress: logs filters/export and no inactive log requests passed.');
  await checkLogLifecycle(server);
  for(const [page,url] of [[server,`http://127.0.0.1:${sa}`],[client,`http://127.0.0.1:${ca}`]]) {
    await page.bringToFront();
    await page.locator('#change-password').click();await page.locator('#password-dialog').waitFor({state:'visible'});
    if(page===server)await page.screenshot({path:path.join(out,'change-password.png'),fullPage:true});
    await changePassword(page,password,newPassword);await page.locator('#login-panel').waitFor({state:'visible'});await login(page,url,newPassword);
  }
  assert.deepEqual(errors,[]);console.log('UI PASS: login, required default-password change, password validation, change-password header action, reauthentication, lazy logs/filter/export, no logs polling outside logs view, in-flight cancellation, visibility pause/resume, create, forwarding, rotation, credential replacement, mobile layout, audit, deletion.');
}
main().catch(e=>{console.error(e);process.exitCode=1;}).finally(async()=>{if(browser)await browser.close();if(service)service.closeAllConnections();if(service)service.close();for(const p of children){if(p.exitCode===null){p.kill();await once(p,'exit').catch(()=>{});}}if(dir){const target=path.resolve(dir);assert.equal(path.dirname(target),path.resolve(os.tmpdir()));assert.ok(path.basename(target).startsWith('my-frp-ui-'));await fs.rm(target,{recursive:true,force:true,maxRetries:5,retryDelay:200});}});
