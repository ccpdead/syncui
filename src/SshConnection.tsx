import { isTauri } from "@tauri-apps/api/core";
import { useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { SshProfile, ConnectionInfo, RemoteDirectory, RemoteEndpoint, loadSshProfiles, saveSshProfiles, connectSsh, deploySshAgent, disconnectSsh, forgetSshPassword, browseSsh, sshAuthReply } from "./api";
const fresh = (): SshProfile => ({ id: crypto.randomUUID(), name: "新服务器", host: "", user: "", port: null, identityFile: "", auth: "auto", rememberPassword: false });
export function SshAuthDialog() {
  const [request, setRequest] = useState<{id:string;prompt:string;kind:string} | null>(null);
  const [answer,setAnswer]=useState("");
  const [error,setError]=useState("");
  useEffect(()=>{if(!isTauri())return;const off=listen<{id:string;prompt:string;kind:string}>("ssh-auth-request",e=>{setRequest(e.payload);setAnswer("");setError("");});return()=>{void off.then(f=>f());};},[]);
  if(!request)return null;
  const reply=async(value:string|null)=>{try{await sshAuthReply(request.id,value);setRequest(null);setAnswer("");}catch(e){setError(String(e));setRequest(null);}};
  return <div className="ssh-modal-backdrop"><section className="ssh-modal" role="dialog" aria-modal="true" aria-labelledby="ssh-auth-title">
    <h3 id="ssh-auth-title">{request.kind==="hostKey"?"确认服务器身份":"SSH 身份验证"}</h3>
    <pre className="ssh-prompt">{request.prompt}</pre>
    {request.kind!=="hostKey"&&<input aria-label="密码或密钥口令" type="password" autoFocus autoComplete="off" value={answer} onChange={e=>setAnswer(e.target.value)} onKeyDown={e=>{if(e.key==="Enter")void reply(answer);}}/>}
    {error&&<p className="error">{error}</p>}
    <div className="ssh-actions"><button className="btn" onClick={()=>void reply(null)}>取消</button><button className="btn primary" onClick={()=>void reply(request.kind==="hostKey"?"yes":answer)}>{request.kind==="hostKey"?"信任并继续":"继续"}</button></div>
  </section></div>;
}
export default function SshConnection({endpoint,onChange,onReady,disabled}:{endpoint:Extract<RemoteEndpoint,{kind:"ssh"}>;onChange:(e:Extract<RemoteEndpoint,{kind:"ssh"}>)=>void;onReady:(ready:boolean)=>void;disabled:boolean}) {
  const [expanded,setExpanded]=useState(true);
  const [profiles,setProfiles]=useState<SshProfile[]>([]);const [profile,setProfile]=useState<SshProfile>(fresh);
  const [password,setPassword]=useState("");const [info,setInfo]=useState<ConnectionInfo|null>(null);const [busy,setBusy]=useState(false);const [message,setMessage]=useState("");const [directory,setDirectory]=useState<RemoteDirectory|null>(null);
  useEffect(()=>{if(!isTauri()){setMessage("网页仅预览界面；连接、部署和同步请使用桌面应用。");return;}let dead=false;void loadSshProfiles().then(list=>{if(dead)return;setProfiles(list);const p=list.find(x=>x.id===endpoint.profileId)??list[0]??fresh();setProfile(p);onChange({...endpoint,profileId:p.id});}).catch(e=>setMessage(String(e)));return()=>{dead=true;};},[]);
  const change=(next:SshProfile)=>{setProfile(next);setInfo(null);onReady(false);};
  const run=async(action:()=>Promise<void>)=>{setBusy(true);setMessage("");try{await action();}catch(e){setMessage(String(e));}finally{setBusy(false);}};
  const remember=async()=>{const next=[...profiles.filter(p=>p.id!==profile.id),profile];await saveSshProfiles(next);setProfiles(next);};
  const connect=async()=>{await remember();const c=await connectSsh(profile,password||undefined);setPassword("");setInfo(c);setExpanded(!c.agentReady);setMessage(c.message);onReady(c.agentReady);if(!endpoint.path)onChange({...endpoint,path:c.home});return c;};
  const locked=disabled||busy;
  return <section className="ssh-panel">
    <div className="ssh-heading"><strong>SSH 服务器</strong><button className="link" onClick={()=>setExpanded(!expanded)}>{expanded?"收起配置":"展开配置"}</button><span>{info?`${info.arch} · ${info.agentReady?"工具就绪":"待部署工具"}`:"未连接"}</span></div>
    {expanded&&<fieldset disabled={locked} className="ssh-fields">
      <label>连接配置<select value={profile.id} onChange={e=>{const p=profiles.find(p=>p.id===e.target.value);if(p){change(p);onChange({...endpoint,profileId:p.id});}}}>{!profiles.some(p=>p.id===profile.id)&&<option value={profile.id}>{profile.name}</option>}{profiles.map(p=><option key={p.id} value={p.id}>{p.name}</option>)}</select></label>
      <button className="btn" onClick={()=>{const p=fresh();change(p);onChange({...endpoint,profileId:p.id,path:""});}}>新建配置</button>
      <label>名称<input value={profile.name} onChange={e=>change({...profile,name:e.target.value})}/></label>
      <label>主机 / SSH 别名<input placeholder="192.168.3.72 或 robot" value={profile.host} onChange={e=>change({...profile,host:e.target.value})}/></label>
      <label>用户名<input placeholder="沿用 SSH 配置" value={profile.user} onChange={e=>change({...profile,user:e.target.value})}/></label>
      <label>端口<input type="number" min={1} max={65535} placeholder="SSH 配置 / 22" value={profile.port??""} onChange={e=>change({...profile,port:e.target.value?Number(e.target.value):null})}/></label>
      <label>认证方式<select value={profile.auth} onChange={e=>change({...profile,auth:e.target.value as SshProfile["auth"]})}><option value="auto">自动 / SSH 配置</option><option value="key">密钥 / ssh-agent</option><option value="password">密码</option></select></label>
      <label>{profile.auth==="key"?"密钥文件（可选）":"密码（可选）"}{profile.auth==="key"?<input placeholder="沿用 SSH 配置" value={profile.identityFile} onChange={e=>change({...profile,identityFile:e.target.value})}/>:<input type="password" autoComplete="new-password" value={password} placeholder="留空可使用已保存密码" onChange={e=>setPassword(e.target.value)}/>}</label>
      <label className="ssh-check"><input type="checkbox" checked={profile.rememberPassword} onChange={e=>change({...profile,rememberPassword:e.target.checked})}/>记住密码（系统密钥环）</label>
      <label className="ssh-root">远端目录<input value={endpoint.path} placeholder="/home/nvidia/ros2_ws" onChange={e=>onChange({...endpoint,path:e.target.value})}/></label>
    </fieldset>}
    {!expanded&&<div className="ssh-summary">{profile.user?`${profile.user}@`:""}{profile.host||"未填写服务器"}<br/>{endpoint.path||"未选择目录"}</div>}
    <div className="ssh-actions">
      <button className="btn primary" disabled={locked||!profile.host} onClick={()=>void run(async()=>{await connect();})}>{busy?"处理中…":"连接并检测"}</button>
      <button className="btn" disabled={locked||!profile.host} onClick={()=>void run(async()=>{const c=info??await connect();const ready=await deploySshAgent(c.id);setInfo(ready);setExpanded(false);onReady(ready.agentReady);setMessage("远端工具已部署。"+ready.message);})}>部署 / 升级工具</button>
      <button className="btn" disabled={locked||!info?.agentReady} onClick={()=>void run(async()=>setDirectory(await browseSsh(profile.id,endpoint.path)))}>浏览目录</button>
      <button className="btn" disabled={locked||!info} onClick={()=>void run(async()=>{await disconnectSsh(profile.id);setInfo(null);onReady(false);})}>断开</button>
      <button className="btn" disabled={locked} onClick={()=>void run(async()=>{await forgetSshPassword(profile);setMessage("已删除密钥环中的密码");})}>删除保存的密码</button>
    </div>
    {info&&<small className="ssh-fingerprint">{info.user} · {info.hostKey}</small>}
    {message&&<p className="ssh-message" role="status">{message}</p>}
    {directory&&<div className="ssh-modal-backdrop"><section className="ssh-modal" role="dialog" aria-modal="true" aria-labelledby="ssh-directory-title"><h3 id="ssh-directory-title">选择远端目录</h3><code>{directory.path}</code><div className="ssh-directory-list">{directory.parent&&<button onClick={()=>void run(async()=>setDirectory(await browseSsh(profile.id,directory.parent!)))}>↑ 上级目录</button>}{directory.children.map(name=><button key={name} onClick={()=>void run(async()=>setDirectory(await browseSsh(profile.id,`${directory.path.replace(/\/$/,"")}/${name}`)))}>▸ {name}</button>)}</div><div className="ssh-actions"><button className="btn" onClick={()=>setDirectory(null)}>取消</button><button className="btn primary" onClick={()=>{onChange({...endpoint,path:directory.path});setDirectory(null);}}>选择此目录</button></div></section></div>}
  </section>;
}
