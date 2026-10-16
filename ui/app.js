'use strict';
const $ = id => document.getElementById(id);
let role = 'server', entries = [], auth = sessionStorage.getItem('myfrp-session') || '', refreshTask, timer, confirmAction;
let mustChangePassword = false, accountVerified = false, activeView = 'tunnels', refreshInterval, logsTask, logsRequestId = 0, logsInterval, logsController, exportController;
const esc = v => String(v ?? '').replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
const date = n => n ? new Date(n * 1000).toLocaleString('zh-CN', {hour12:false}) : '—';
const bytes = n => { n = Number(n) || 0; const units = ['B','KB','MB','GB','TB']; let i=0; while(n>=1024 && i<4){n/=1024;i++;} return `${n.toFixed(i?1:0)} ${units[i]}`; };
function toast(message){ $('toast').textContent=message; $('toast').classList.remove('hidden'); clearTimeout(timer); timer=setTimeout(()=>$('toast').classList.add('hidden'),4500); }
async function api(path,method='GET',body,options={}){
  const session=auth;
  const response=await fetch(path,{...options,method,headers:{'Content-Type':'application/json',...(session?{'Authorization':`Bearer ${session}`}:{})},body:body===undefined?undefined:JSON.stringify(body)});
  const data=await response.json();
  if(!response.ok)throw requestError(response,data,session,path);return data;
}
function requestError(response,data,session,path){
  const error=new Error(data.error||`请求失败 (${response.status})`);
  if(session&&auth===session&&path!='/api/login'){
    if(response.status===401)signedOut();
    if(response.status===403&&data.must_change_password){error.mustChangePassword=true;requirePasswordChange();}
  }
  return error;
}
function showAuthenticated(){
  $('login-panel').classList.add('hidden');$('dashboard').classList.toggle('hidden',mustChangePassword);
  $('logout').classList.remove('hidden');$('change-password').classList.remove('hidden');
}
function signedOut(){
  auth='';accountVerified=false;mustChangePassword=false;sessionStorage.removeItem('myfrp-session');clearInterval(refreshInterval);refreshInterval=undefined;
  stopLogService();
  $('dashboard').classList.add('hidden');$('logout').classList.add('hidden');$('change-password').classList.add('hidden');$('login-panel').classList.remove('hidden');$('refresh-label').textContent='等待登录';
  for(const d of document.querySelectorAll('dialog'))d.close();
  $('issued-token').value='';$('token-input').value='';$('password-form').reset();$('password-error').textContent='';$('logs-error').textContent='';$('logs-list').innerHTML='<div class="empty">暂无运行日志</div>';logsRequestId++;entries=[];setView('tunnels',false);
}
function startPolling(){clearInterval(refreshInterval);refreshInterval=setInterval(()=>{if(!auth||!accountVerified||mustChangePassword)return;refresh();},3000);}
function requirePasswordChange(){mustChangePassword=true;accountVerified=true;clearInterval(refreshInterval);stopLogService();showAuthenticated();$('refresh-label').textContent='请先修改初始密码';openPasswordDialog(true);}
function openPasswordDialog(forced=false){
  if(!auth)return;
  if(forced)mustChangePassword=true;
  const dialog=$('password-dialog');
  if(!dialog.open){$('password-form').reset();$('password-error').textContent='';}
  $('password-title').textContent=mustChangePassword?'请修改初始密码':'修改管理密码';
  $('password-description').textContent=mustChangePassword?'当前使用初始密码。请先设置至少 12 个字符的新密码，再继续使用控制台。修改后需重新登录。':'修改后所有管理会话将退出，请使用新密码重新登录。';
  $('password-close').classList.toggle('hidden',mustChangePassword);$('password-cancel').classList.toggle('hidden',mustChangePassword);$('password-logout').classList.toggle('hidden',!mustChangePassword);
  if(!dialog.open)dialog.showModal();
}
function refresh(force=false){
  if(!auth||!accountVerified||mustChangePassword)return Promise.resolve();
  if(refreshTask)return force?refreshTask.then(()=>refresh(true)):refreshTask;
  refreshTask=loadEntries().finally(()=>{refreshTask=null;});return refreshTask;
}
async function loadEntries(){
  const session=auth;
  try{const data=await api('/api/tunnels');if(auth!==session||mustChangePassword)return;entries=data.items;showAuthenticated();
    $('total').textContent=entries.length;$('count').textContent=entries.length;
    $('online').textContent=entries.filter(e=>role==='server'?e.online:e.status.state==='在线').length;
    $('active').textContent=entries.reduce((n,e)=>n+e.status.active,0);
    $('traffic').textContent=bytes(entries.reduce((n,e)=>n+e.status.sent+e.status.received,0));
    $('empty').classList.toggle('hidden',entries.length!==0);
    $('endpoint').textContent=role==='server'?`客户端接入 ${data.public_url} · 可用端口 ${data.port_min}–${data.port_max}`:'本机连接独立运行，可接入不同服务端';
    $('refresh-label').textContent=`更新于 ${new Date().toLocaleTimeString('zh-CN',{hour12:false})}`;
    $('rows').innerHTML=entries.map(({config:c,status:s,online,expired})=>{
      let state=role==='server'?(!c.enabled?'已禁用':expired?'凭证过期':s.state==='监听失败'?s.state:online?'在线':'等待客户端'):s.state;
      const good=state==='在线', off=['已禁用','已停止','等待客户端'].includes(state);
      const address=role==='server'?`${c.bind_ip}:${c.port}`:c.local_addr;
      return `<tr><td><strong>${esc(c.name)}</strong><small>${esc(c.id.slice(0,8))} · TCP</small></td><td><strong>${esc(address)}</strong><small>${esc(role==='server'?'公网映射入口':(c.endpoint||'凭证已过期')+(s.public_port?' → '+s.public_port:''))}</small></td><td><span class="badge ${good?'':off?'off':'warn'}">${esc(state)}</span><small title="${esc(s.error||'')}">${esc(s.error||(role==='client'?'本地服务：'+s.local:s.last_seen?'心跳 '+date(s.last_seen):s.state))}</small></td><td>${esc(date(c.expires_at))}</td><td>${bytes(s.sent+s.received)}<small>${s.active} 个活动连接</small></td><td><div class="row-actions"><button data-action="edit" data-id="${esc(c.id)}">编辑</button><button data-action="toggle" data-id="${esc(c.id)}">${c.enabled?(role==='server'?'禁用':'停止'):'启用'}</button>${role==='server'?`<button data-action="rotate" data-id="${esc(c.id)}">刷新凭证</button>`:''}<button class="danger" data-action="delete" data-id="${esc(c.id)}">删除</button></div></td></tr>`;
    }).join('');
  }catch(e){if(auth===session&&!e.mustChangePassword)$('refresh-label').textContent=`刷新失败：${e.message}`;}
}
function edit(id){
  $('edit-form').reset();$('form-error').textContent='';$('entry-id').value=id||'';$('edit-title').textContent=id?'编辑隧道':'新建隧道';
  $('ttl-label').classList.toggle('hidden',!!id);$('server-fields').classList.toggle('hidden',role!=='server');$('client-fields').classList.toggle('hidden',role!=='client');
  $('local-addr').required=role==='client';$('token-input').required=role==='client'&&!id;
  for(const field of ['bind-ip','port','max-connections','ttl'])$(field).required=role==='server'&&(field!=='ttl'||!id);
  if(id){const c=entries.find(e=>e.config.id===id).config;$('name').value=c.name;$('enabled').checked=c.enabled;if(role==='server'){$('bind-ip').value=c.bind_ip;$('port').value=c.port;$('max-connections').value=c.max_connections;}else{$('local-addr').value=c.local_addr;}}
  $('token-preview').textContent='服务端地址将从凭证中读取。';$('editor').showModal();
}
function payload(){return role==='server'?{name:$('name').value.trim(),bind_ip:$('bind-ip').value.trim(),port:Number($('port').value),max_connections:Number($('max-connections').value),ttl_days:Number($('ttl').value),enabled:$('enabled').checked}:{name:$('name').value.trim(),local_addr:$('local-addr').value.trim(),token:$('token-input').value.trim(),enabled:$('enabled').checked};}
function showToken(token){$('issued-token').value=token;$('token-dialog').showModal();}
function confirm(title,text,action,rotate=false){$('confirm-title').textContent=title;$('confirm-text').textContent=text;$('confirm-error').textContent='';$('rotate-ttl-label').classList.toggle('hidden',!rotate);$('rotate-ttl').value=30;confirmAction=action;$('confirm-dialog').showModal();}
$('login-form').addEventListener('submit',async e=>{e.preventDefault();const button=e.submitter;button.disabled=true;try{const data=await api('/api/login','POST',{password:$('password').value});auth=data.token;accountVerified=true;mustChangePassword=Boolean(data.must_change_password);sessionStorage.setItem('myfrp-session',auth);$('password').value='';showAuthenticated();if(mustChangePassword)requirePasswordChange();else{startPolling();await refresh();}}catch(e){toast(e.message);}finally{button.disabled=false;}});
async function logout(){try{await api('/api/logout','POST',{});}catch{}signedOut();}
$('logout').onclick=$('password-logout').onclick=logout;
$('change-password').onclick=()=>openPasswordDialog();
function closePasswordDialog(){if(!mustChangePassword)$('password-dialog').close();}
$('password-close').onclick=$('password-cancel').onclick=closePasswordDialog;
$('password-dialog').addEventListener('cancel',e=>{if(mustChangePassword)e.preventDefault();});
$('password-dialog').addEventListener('close',()=>{$('password-form').reset();$('password-error').textContent='';});
$('password-form').addEventListener('submit',async e=>{
  e.preventDefault();$('password-error').textContent='';
  if(Array.from($('new-password').value).length<12){$('password-error').textContent='新密码至少需要 12 个字符。';return;}
  if($('new-password').value!==$('confirm-password').value){$('password-error').textContent='两次输入的新密码不一致。';return;}
  const button=e.submitter,session=auth;button.disabled=true;
  try{await api('/api/password','POST',{current_password:$('current-password').value,new_password:$('new-password').value,confirm_password:$('confirm-password').value});if(auth===session){signedOut();toast('密码已修改，请使用新密码重新登录。');$('password').focus();}}
  catch(e){if(auth===session)$('password-error').textContent=e.message;}
  finally{button.disabled=false;}
});
$('add').onclick=$('empty-add').onclick=()=>edit();
document.querySelectorAll('.close-editor').forEach(b=>b.onclick=()=>$('editor').close());
$('edit-form').addEventListener('submit',async e=>{e.preventDefault();const b=e.submitter;b.disabled=true;try{const id=$('entry-id').value;const result=await api('/api/tunnels'+(id?'/'+id:''),id?'PUT':'POST',payload());$('token-input').value='';await refresh(true);$('editor').close();if(result.token)showToken(result.token);else toast('配置已保存');}catch(e){$('form-error').textContent=e.message;}finally{b.disabled=false;}});
$('rows').addEventListener('click',async event=>{const button=event.target.closest('[data-action]');if(!button)return;const {id,action}=button.dataset;const entry=entries.find(e=>e.config.id===id);if(!entry)return;const c=entry.config;
  if(action==='edit')return edit(id);
  if(action==='rotate')return confirm('刷新连接凭证','旧凭证将立即失效，现有连接会断开。请将新凭证替换到客户端。',async()=>{const r=await api(`/api/tunnels/${id}/rotate`,'POST',{ttl_days:Number($('rotate-ttl').value)});showToken(r.token);},true);
  if(action==='delete')return confirm('删除隧道',`确定删除“${c.name}”？现有连接将关闭。`,()=>api(`/api/tunnels/${id}`,'DELETE'));
  const update=()=>api(`/api/tunnels/${id}`,'PUT',{...c,enabled:!c.enabled,ttl_days:30,token:''});
  if(c.enabled)return confirm(role==='server'?'禁用隧道':'停止连接','此操作将断开这条隧道的现有连接。',update);
  try{await update();await refresh(true);}catch(e){toast(e.message);}
});
$('confirm-form').addEventListener('submit',async e=>{e.preventDefault();const b=e.submitter;b.disabled=true;try{await confirmAction();await refresh(true);$('confirm-dialog').close();}catch(e){$('confirm-error').textContent=e.message;}finally{b.disabled=false;}});
$('cancel-confirm').onclick=()=>$('confirm-dialog').close();
$('close-token').onclick=()=>$('token-dialog').close();$('token-dialog').addEventListener('close',()=>$('issued-token').value='');
$('copy-token').onclick=async()=>{try{await navigator.clipboard.writeText($('issued-token').value);toast('凭证已复制');}catch{$('issued-token').select();toast('请按 Ctrl+C 复制凭证');}};
$('token-input').addEventListener('input',()=>{try{const part=$('token-input').value.trim().split('.')[1].replace(/-/g,'+').replace(/_/g,'/');const c=JSON.parse(new TextDecoder().decode(Uint8Array.from(atob(part),x=>x.charCodeAt(0))));$('token-preview').textContent=`目标：${c.endpoint} · 到期：${date(c.exp)}（尚未认证）`;}catch{$('token-preview').textContent='请粘贴完整的 JWT 连接凭证。';}});
async function audit(){const session=auth;try{const rows=await api('/api/audit');if(auth!==session||mustChangePassword)return;const names={create:'创建隧道',update:'更新配置',delete:'删除隧道',rotate:'刷新凭证',login:'管理员登录',password_change:'修改管理密码',change_password:'修改管理密码',password_reset:'重置管理密码',password_initialize:'初始化管理密码',logout:'退出登录'};$('audit-list').innerHTML=rows.map(r=>`<div class="audit-row"><time>${esc(date(r.at))}</time><strong>${esc(names[r.action]||r.action)}</strong><span>${esc(r.target)}</span></div>`).join('')||'<div class="empty">暂无操作记录</div>';}catch(e){if(auth===session&&!e.mustChangePassword)toast(e.message);}}
function setView(view,load=true){
  stopLogService();
  activeView=view;
  for(const name of ['tunnels','audit','logs']){$(name==='tunnels'?'tunnel-section':`${name}-section`).classList.toggle('hidden',name!==view);$(`nav-${name}`).classList.toggle('active',name===view);}
  $('heading').textContent={tunnels:'隧道管理',audit:'操作记录',logs:'运行日志'}[view];
  $('subtitle').textContent=view==='logs'?'从启动到连接，查看服务的运行情况。':view==='audit'?'查看管理操作及配置变更。':role==='client'?'管理本地服务，连接不同公网节点。':'让本地服务，安全连接更远的地方。';
  if(load&&auth&&accountVerified&&!mustChangePassword){if(view==='audit')audit();if(view==='logs')startLogService();}
}
for(const view of ['tunnels','audit','logs'])$(`nav-${view}`).onclick=()=>{if(auth&&accountVerified&&!mustChangePassword)setView(view);};
$('reload-audit').onclick=audit;
function logsQuery(){const query=new URLSearchParams({limit:'200'});if($('log-level').value)query.set('level',$('log-level').value);if($('log-query').value.trim())query.set('q',$('log-query').value.trim());return query.toString();}
function canReadLogs(){return Boolean(auth&&accountVerified&&!mustChangePassword&&activeView==='logs'&&!document.hidden);}
function stopLogService(){clearInterval(logsInterval);logsInterval=undefined;logsController?.abort();exportController?.abort();logsController=undefined;exportController=undefined;logsTask=undefined;logsRequestId++;}
function startLogService(){
  if(!canReadLogs()){stopLogService();return;}
  clearInterval(logsInterval);logsInterval=undefined;loadLogs(true);
  if($('log-auto-refresh').checked)logsInterval=setInterval(()=>{if(canReadLogs())loadLogs();else stopLogService();},5000);
}
function loadLogs(force=false){
  if(!canReadLogs())return Promise.resolve();
  if(logsTask&&!force)return logsTask;
  if(force)logsController?.abort();
  const controller=new AbortController();logsController=controller;
  const requestId=++logsRequestId,session=auth,query=logsQuery();$('logs-error').textContent='';$('logs-state').textContent='正在读取日志…';
  const task=(async()=>{try{
    const data=await api(`/api/logs?${query}`,'GET',undefined,{signal:controller.signal});if(auth!==session||requestId!==logsRequestId||!canReadLogs())return;
    $('logs-list').innerHTML=data.items.map(r=>{const level=['ERROR','WARN','INFO','DEBUG'].includes(r.level)?r.level:'INFO';return `<div class="log-entry"><time>${esc(date(r.at))}</time><span class="log-level level-${level.toLowerCase()}">${esc(r.level)}</span><span class="log-component">${esc(r.component)}${r.tunnel_id?`<small title="隧道标识">${esc(r.tunnel_id)}</small>`:''}</span><span class="log-message">${esc(r.message)}</span></div>`;}).join('')||'<div class="empty"><h3>暂无匹配的运行日志</h3><p>可调整日志级别或关键词后重试。</p></div>';
    $('logs-state').textContent=`最近 ${data.items.length} 条 · 更新于 ${new Date().toLocaleTimeString('zh-CN',{hour12:false})}`;
  }catch(e){if(e.name!=='AbortError'&&auth===session&&requestId===logsRequestId&&canReadLogs()&&!e.mustChangePassword){$('logs-error').textContent=`读取失败：${e.message}`;$('logs-state').textContent='日志读取失败，可点击刷新重试';}}
  finally{if(logsTask===task)logsTask=undefined;if(logsController===controller)logsController=undefined;}})();logsTask=task;return task;
}
$('logs-filter').addEventListener('submit',e=>{e.preventDefault();loadLogs(true);});
$('log-level').onchange=()=>loadLogs(true);
$('log-auto-refresh').onchange=()=>{clearInterval(logsInterval);logsInterval=undefined;if($('log-auto-refresh').checked&&canReadLogs())startLogService();};
document.addEventListener('visibilitychange',()=>{if(document.hidden)stopLogService();else if(canReadLogs())startLogService();});
window.addEventListener('pagehide',stopLogService);
$('log-export').onclick=async e=>{
  if(!canReadLogs())return;
  const button=e.currentTarget,session=auth;button.disabled=true;$('logs-error').textContent='';
  const controller=new AbortController();exportController?.abort();exportController=controller;
  try{const response=await fetch(`/api/logs/export?${logsQuery()}`,{headers:{Authorization:`Bearer ${session}`},signal:controller.signal});
    if(!response.ok){const data=await response.json();throw requestError(response,data,session,'/api/logs/export');}
    const blob=await response.blob();if(auth!==session||!canReadLogs())return;
    const url=URL.createObjectURL(blob),link=document.createElement('a');link.href=url;link.download=`my-frp-${role}-logs-${new Date().toISOString().replace(/[:.]/g,'-')}.log`;document.body.appendChild(link);link.click();link.remove();setTimeout(()=>URL.revokeObjectURL(url),1000);
  }catch(e){if(e.name!=='AbortError'&&auth===session&&canReadLogs()&&!e.mustChangePassword)$('logs-error').textContent=`导出失败：${e.message}`;}
  finally{if(exportController===controller)exportController=undefined;button.disabled=false;}
};
(async()=>{try{
  const meta=await api('/api/meta');role=meta.role;const label=role==='server'?'服务端控制台':'客户端控制台';$('role-label').textContent=label;$('footer-role').textContent=label;$('version').textContent=`v${meta.version} · Embedded console`;document.title=`my-frp · ${label}`;
  if(role==='client'){$('empty-copy').textContent='填写本地服务地址和服务端凭证，即可建立连接。';$('address-heading').textContent='本地服务 / 服务端';}setView('tunnels',false);
  if(auth){const session=auth;const account=await api('/api/account');if(auth!==session)return;accountVerified=true;mustChangePassword=Boolean(account.must_change_password);showAuthenticated();if(mustChangePassword)requirePasswordChange();else{startPolling();await refresh();}}
}catch(e){if(!e.mustChangePassword)toast(e.message);}})();
